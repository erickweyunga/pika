//! The high-level intermediate representation (HIR) of a Pika program.
//!
//! [`lower_program`] turns the parsed modules of a program's packages into a [`Module`]:
//! functions, types, constants and globals whose bodies are arenas of expressions and
//! statements ([`lower()`] does it for a program of one file). Every name is resolved during
//! lowering, including paths to the items of other modules, so later phases never deal with
//! scopes or strings: a variable read is a [`LocalId`], a call names a [`FnId`] or a
//! [`Builtin`], and a type annotation is a [`Ty`]. Spans are kept for every node, for
//! diagnostics; they are offsets in the program's source map, which tell their file.

pub mod codes;
mod lower;

pub use la_arena::{Arena, ArenaMap, Idx};
pub use lower::{Root, SINGLE_FILE_PACKAGE, SourceModule, SourcePackage, lower, lower_program};
use pika_diagnostics::Span;
pub use pika_syntax::ast::{BinaryOp, Convention, PrefixOp as UnaryOp};
pub use pika_ty::{Adt, AdtKind, FloatTy, IntTy, Param as TyParam, Ty};

/// Identifies a module (one source file) in a [`Module`].
pub type ModuleId = Idx<ModuleDef>;
/// Identifies a function in a [`Module`].
pub type FnId = Idx<Function>;
/// Identifies a user-defined type (a struct or an enum) in a [`Module`].
pub type TypeId = Idx<TypeDef>;
/// Identifies a [`TraitDef`] in a [`Module`].
pub type TraitId = Idx<TraitDef>;
/// Identifies an [`ImplDef`] in a [`Module`].
pub type ImplId = Idx<ImplDef>;
/// Identifies a module-level constant in a [`Module`].
pub type ConstId = Idx<ConstItem>;
/// Identifies a module-level variable in a [`Module`].
pub type GlobalId = Idx<GlobalItem>;
/// Identifies an expression in a [`Body`].
pub type ExprId = Idx<Expr>;
/// Identifies a statement in a [`Body`].
pub type StmtId = Idx<Stmt>;
/// Identifies a local variable or parameter in a [`Body`].
pub type LocalId = Idx<Local>;
/// Identifies a pattern in a [`Body`].
pub type PatId = Idx<Pat>;

/// A value with the span of the source it came from.
#[derive(Clone, Debug, PartialEq)]
pub struct Spanned<T> {
    /// The value.
    pub value: T,
    /// Where it was written.
    pub span: Span,
}

/// A lowered program: the items of every module of every package in it, with each name
/// resolved.
#[derive(Clone, Debug, Default)]
pub struct Module {
    /// The modules: one per source file, and the prelude's, which has the empty path.
    pub modules: Arena<ModuleDef>,
    /// The root module of the package being compiled; the other packages are its
    /// dependencies.
    pub root: Option<ModuleId>,
    /// Functions, including the implicit `main` of a script and the methods of structs.
    pub functions: Arena<Function>,
    /// User-defined types: structs and enums.
    pub types: Arena<TypeDef>,
    /// User-defined traits, and the prelude traits.
    pub traits: Arena<TraitDef>,
    /// The functions of built-in types, declared with `:impl`.
    pub impls: Arena<ImplDef>,
    /// The `Error` type that `raises` functions raise (spec section 9), declared by the
    /// language as a struct.
    pub error_type: Option<TypeId>,
    /// Module-level constants (`:const` at the top level).
    pub consts: Arena<ConstItem>,
    /// Module-level variables (`:global`).
    pub globals: Arena<GlobalItem>,
    /// The program's entry point: an explicit `:fn main`, or the implicit `main` of a script.
    pub entry: Option<FnId>,
    /// The tests, in declaration order.
    pub tests: Vec<TestDef>,
}

/// The name of the standard library's package, which every package can use.
pub const STD_PACKAGE: &str = "std";

/// Returns true if `name`, of an item or a field, makes it private to its module (spec
/// section 13.3).
pub fn is_private(name: &str) -> bool {
    name.starts_with('_')
}

/// A module: one source file of a package (spec section 13.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ModuleDef {
    /// The module's absolute path: its package's name, then the path of its file in the
    /// package's `src` directory, as in `["hello", "net", "http"]` for `src/net/http.pk`.
    pub path: Vec<String>,
}

impl ModuleDef {
    /// The path as written in Pika, as in `/hello/net/http`.
    pub fn display_path(&self) -> String {
        self.path.iter().fold(String::new(), |mut path, segment| {
            path.push('/');
            path.push_str(segment);
            path
        })
    }
}

impl Module {
    /// Returns true if the function `id` is in the package being compiled, rather than in a
    /// dependency or the prelude.
    pub fn is_local(&self, id: FnId) -> bool {
        self.in_root_package(self.functions[id].module)
    }

    /// Returns true if the module `id` is in the package being compiled.
    pub fn in_root_package(&self, id: ModuleId) -> bool {
        let package = |module: ModuleId| self.modules[module].path.first();
        self.root.is_some_and(|root| package(id) == package(root))
    }

    /// The function `name` that an `:impl` gives the built-in types with head `head`.
    pub fn impl_function(&self, head: TypeHead, name: &str) -> Option<(ImplId, FnId)> {
        self.impls
            .iter()
            .filter(|(_, def)| def.head == Some(head))
            .find_map(|(id, def)| def.function(name).map(|function| (id, function)))
    }

    /// The prelude trait `which`.
    ///
    /// # Panics
    ///
    /// If the module was not built by [`lower_program`], which declares every prelude trait.
    pub fn prelude_trait(&self, which: PreludeTrait) -> TraitId {
        self.traits
            .iter()
            .find(|(_, def)| def.prelude == Some(which))
            .map(|(id, _)| id)
            .expect("lowering declares every prelude trait")
    }
}

/// Whether a function was written by the user or synthesized.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnKind {
    /// Declared with `:fn`.
    Declared,
    /// The top-level statements of a script (spec section 4.4).
    ImplicitMain,
    /// An anonymous function, `[:fn ...]`: the body of a closure, created in the body of the
    /// given function. It has the type parameters of that function.
    Closure(FnId),
    /// An intrinsic of the runtime, declared by the standard library in
    /// `:extern lib="pika" {...}`. It has no body: the runtime implements it.
    Runtime,
    /// The body of a `:test`, which raises the errors it does not catch.
    Test,
}

/// A test: `:test "name" do={...}` (spec section 14).
#[derive(Clone, Debug)]
pub struct TestDef {
    /// The test's name.
    pub name: Spanned<String>,
    /// The module that declares it.
    pub module: ModuleId,
    /// The function of its body.
    pub function: FnId,
}

/// A variable captured by a closure.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Capture {
    /// The variable in the parent's body.
    pub outer: LocalId,
    /// The variable in the closure's body, which cannot be modified.
    pub inner: LocalId,
}

/// A function.
#[derive(Clone, Debug)]
pub struct Function {
    /// The function's name.
    pub name: Spanned<String>,
    /// The module that declares it.
    pub module: ModuleId,
    /// Whether the function was declared or synthesized.
    pub kind: FnKind,
    /// The type or trait that declares this function, for methods and associated functions.
    pub owner: Option<FnOwner>,
    /// The type parameters in scope: those of the owner type, then the function's own,
    /// which start at `own_generics`.
    pub generics: Generics,
    /// The position of the function's own first type parameter in `generics`.
    pub own_generics: usize,
    /// The parameters, in order.
    pub params: Vec<Param>,
    /// The declared return type (`nothing` when omitted).
    pub ret: Spanned<Ty>,
    /// Whether the function is declared `raises`.
    pub raises: bool,
    /// For a closure: the variables of the parent captured by value when the closure is
    /// created, each with the variable that holds it in the closure's body.
    pub captures: Vec<Capture>,
    /// Whether the function has a body: false only for the required functions of a trait,
    /// which the types implementing it define.
    pub has_body: bool,
    /// The body. Parameters are its first locals.
    pub body: Body,
    /// The function's statements.
    pub root: Block,
}

/// What declares a function of a type or trait.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FnOwner {
    /// A struct or enum. The function's first type parameters are the type's.
    Type(TypeId),
    /// A trait. The function's first type parameter is `Self`, the implementing type.
    Trait(TraitId),
    /// An `:impl` of a built-in type. The function's first type parameters are the impl's.
    Impl(ImplId),
}

/// Functions of a built-in type, declared with `:impl` in the standard library.
#[derive(Clone, Debug)]
pub struct ImplDef {
    /// The module that declares them.
    pub module: ModuleId,
    /// The impl's type parameters, which its functions have first.
    pub generics: Generics,
    /// The type the functions are for, with the impl's type parameters in it, as `List<T>`.
    pub self_ty: Spanned<Ty>,
    /// The head of the type, if it is a built-in type.
    pub head: Option<TypeHead>,
    /// The functions, by name, in declaration order.
    pub functions: Vec<(String, FnId)>,
}

impl ImplDef {
    /// The function with the given name.
    pub fn function(&self, name: &str) -> Option<FnId> {
        self.functions
            .iter()
            .find(|(n, _)| n == name)
            .map(|&(_, id)| id)
    }
}

/// A built-in type without its type arguments: what an `:impl` adds functions to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TypeHead {
    /// A type without arguments: a number type, `bool`, `char`, `String` or `Duration`.
    Scalar(Ty),
    /// `List<T>`.
    List,
    /// `Map<K, V>`.
    Map,
    /// `Set<T>`.
    Set,
    /// `T?`.
    Option,
    /// `Box<T>`.
    Box,
}

impl TypeHead {
    /// The head of a type, if it is a built-in type that `:impl` can add functions to.
    pub fn of(ty: Ty) -> Option<Self> {
        Some(match ty {
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::String | Ty::Duration => {
                Self::Scalar(ty)
            }
            Ty::List(_) => Self::List,
            Ty::Map(_) => Self::Map,
            Ty::Set(_) => Self::Set,
            Ty::Option(_) => Self::Option,
            Ty::Box(_) => Self::Box,
            _ => return None,
        })
    }

    /// The head named `name` where a type is written, as `List` or `i64`.
    pub fn named(name: &str) -> Option<Self> {
        Some(match name {
            "List" => Self::List,
            "Map" => Self::Map,
            "Set" => Self::Set,
            "Option" => Self::Option,
            "Box" => Self::Box,
            _ => return Ty::from_name(name).and_then(Self::of),
        })
    }
}

/// A function parameter.
#[derive(Clone, Debug)]
pub struct Param {
    /// The local variable that holds the parameter in the body.
    pub local: LocalId,
    /// True for the `self` parameter of a method, which is always the first.
    pub is_self: bool,
    /// How arguments are passed.
    pub convention: Convention,
    /// The declared type.
    pub ty: Spanned<Ty>,
    /// The default value, an expression in the function's body.
    pub default: Option<ExprId>,
}

/// A module-level constant.
#[derive(Clone, Debug)]
pub struct ConstItem {
    /// The constant's name.
    pub name: Spanned<String>,
    /// The module that declares it.
    pub module: ModuleId,
    /// The declared type, if any.
    pub ty: Option<Spanned<Ty>>,
    /// The expressions of the initializer.
    pub body: Body,
    /// The initializer, absent only after an error.
    pub init: Option<ExprId>,
}

/// A module-level variable.
#[derive(Clone, Debug)]
pub struct GlobalItem {
    /// The variable's name.
    pub name: Spanned<String>,
    /// The module that declares it.
    pub module: ModuleId,
    /// The declared type, absent only after an error.
    pub ty: Option<Spanned<Ty>>,
    /// The expressions of the initializer.
    pub body: Body,
    /// The initializer, absent only after an error.
    pub init: Option<ExprId>,
}

/// The expressions, statements and locals of one function or initializer.
#[derive(Clone, Debug, Default)]
pub struct Body {
    /// Expressions.
    pub exprs: Arena<Expr>,
    /// Where each expression was written.
    pub expr_spans: ArenaMap<ExprId, Span>,
    /// Statements.
    pub stmts: Arena<Stmt>,
    /// Where each statement was written.
    pub stmt_spans: ArenaMap<StmtId, Span>,
    /// Local variables, including parameters.
    pub locals: Arena<Local>,
    /// Patterns of `:match` arms.
    pub pats: Arena<Pat>,
    /// Where each pattern was written.
    pub pat_spans: ArenaMap<PatId, Span>,
    /// The head of each call written in the source, by the expression it lowers to.
    pub call_heads: ArenaMap<ExprId, CallHead>,
}

/// The head of a call as written: what names the function, and the `?` that marks a call
/// that can raise an error (spec section 9.1).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CallHead {
    /// The head's text, as `/fs/read` or `$text->parse_int`.
    pub text: String,
    /// Where the head is.
    pub span: Span,
    /// The `?` joined to the head, if written.
    pub mark: Option<Span>,
}

impl Body {
    /// The span of an expression.
    pub fn expr_span(&self, expr: ExprId) -> Span {
        self.expr_spans[expr]
    }

    /// The span of a statement.
    pub fn stmt_span(&self, stmt: StmtId) -> Span {
        self.stmt_spans[stmt]
    }

    /// The span of a pattern.
    pub fn pat_span(&self, pat: PatId) -> Span {
        self.pat_spans[pat]
    }
}

/// Type arguments written after a name, as in `Pair<i64, String>`, with their span.
pub type TypeArgs = Option<Spanned<Vec<Ty>>>;

/// The type parameters of a generic declaration.
#[derive(Clone, Debug, Default)]
pub struct Generics {
    /// The parameters, in order.
    pub params: Vec<GenericParam>,
}

impl Generics {
    /// The parameters as types, in order: the identity substitution.
    pub fn tys(&self) -> Vec<Ty> {
        self.params.iter().map(|p| p.ty).collect()
    }

    /// Returns true if there are no type parameters.
    pub fn is_empty(&self) -> bool {
        self.params.is_empty()
    }
}

/// A type parameter and the traits its types must implement.
#[derive(Clone, Debug)]
pub struct GenericParam {
    /// The parameter's name.
    pub name: Spanned<String>,
    /// The parameter as a type.
    pub ty: Ty,
    /// The bounds, as in `T: Ord + Copy`.
    pub bounds: Vec<Spanned<Bound>>,
}

/// A trait that the types of a type parameter must implement.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Bound {
    /// A built-in trait (spec section 10.4).
    Builtin(BuiltinTrait),
    /// A user-defined trait.
    Trait(TraitId),
}

/// The built-in traits that can bound type parameters.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs, reason = "the variants are the trait names")]
pub enum BuiltinTrait {
    Copy,
    Clone,
    Eq,
    Ord,
    Hash,
    Display,
    Default,
}

impl BuiltinTrait {
    /// The built-in trait with the given name.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "Copy" => Self::Copy,
            "Clone" => Self::Clone,
            "Eq" => Self::Eq,
            "Ord" => Self::Ord,
            "Hash" => Self::Hash,
            "Display" => Self::Display,
            "Default" => Self::Default,
            _ => return None,
        })
    }

    /// The trait's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Copy => "Copy",
            Self::Clone => "Clone",
            Self::Eq => "Eq",
            Self::Ord => "Ord",
            Self::Hash => "Hash",
            Self::Display => "Display",
            Self::Default => "Default",
        }
    }
}

/// A user-defined type: a struct or an enum.
#[derive(Clone, Debug)]
pub struct TypeDef {
    /// The type's name.
    pub name: Spanned<String>,
    /// The module that declares it.
    pub module: ModuleId,
    /// The type.
    pub ty: Ty,
    /// Whether it is a struct or an enum.
    pub kind: AdtKind,
    /// The type parameters.
    pub generics: Generics,
    /// The fields of a struct, in declaration order (none for an enum).
    pub fields: Vec<FieldDef>,
    /// The variants of an enum, in declaration order (none for a struct).
    pub variants: Vec<VariantDef>,
    /// The built-in traits listed in `impl=` (spec section 10.4).
    pub derives: Derives,
    /// The user-defined traits listed in `impl=` (spec section 10.3).
    pub traits: Vec<Spanned<TraitId>>,
    /// Methods and associated functions, by name.
    pub functions: Vec<(String, FnId)>,
    /// The expressions of the fields' default values.
    pub body: Body,
}

/// A variant of an enum.
#[derive(Clone, Debug)]
pub struct VariantDef {
    /// The variant's name.
    pub name: Spanned<String>,
    /// The fields, in order. Variant fields have no default values.
    pub fields: Vec<FieldDef>,
}

impl TypeDef {
    /// The variant with the given name and its index.
    pub fn variant(&self, name: &str) -> Option<(usize, &VariantDef)> {
        self.variants
            .iter()
            .enumerate()
            .find(|(_, v)| v.name.value == name)
    }

    /// The field with the given name and its index.
    pub fn field(&self, name: &str) -> Option<(usize, &FieldDef)> {
        self.fields
            .iter()
            .enumerate()
            .find(|(_, f)| f.name.value == name)
    }

    /// The method or associated function with the given name.
    pub fn function(&self, name: &str) -> Option<FnId> {
        self.functions
            .iter()
            .find(|(n, _)| n == name)
            .map(|&(_, id)| id)
    }
}

/// A field of a struct or of an enum variant.
#[derive(Clone, Debug)]
pub struct FieldDef {
    /// The field's name.
    pub name: Spanned<String>,
    /// The field's type.
    pub ty: Spanned<Ty>,
    /// The default value, an expression in the struct's body.
    pub default: Option<ExprId>,
}

/// A trait whose functions the language calls: for operators and destruction (spec section
/// 10.4). Each is declared in every module, as if written in Pika.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs, reason = "the variants are the trait names")]
pub enum PreludeTrait {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Neg,
    Concat,
    Drop,
}

impl PreludeTrait {
    /// Every prelude trait.
    pub const ALL: [Self; 8] = [
        Self::Add,
        Self::Sub,
        Self::Mul,
        Self::Div,
        Self::Rem,
        Self::Neg,
        Self::Concat,
        Self::Drop,
    ];

    /// The trait's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::Add => "Add",
            Self::Sub => "Sub",
            Self::Mul => "Mul",
            Self::Div => "Div",
            Self::Rem => "Rem",
            Self::Neg => "Neg",
            Self::Concat => "Concat",
            Self::Drop => "Drop",
        }
    }

    /// The name of the trait's only function.
    pub fn function(self) -> &'static str {
        match self {
            Self::Add => "add",
            Self::Sub => "sub",
            Self::Mul => "mul",
            Self::Div => "div",
            Self::Rem => "rem",
            Self::Neg => "neg",
            Self::Concat => "concat",
            Self::Drop => "drop",
        }
    }

    /// The signature of the trait's function, as an implementing type declares it.
    pub fn signature(self) -> String {
        let name = self.function();
        match self {
            Self::Drop => format!(":fn {name} mut self"),
            Self::Neg => format!(":fn {name} self -> Self"),
            _ => format!(":fn {name} self other:Self -> Self"),
        }
    }

    /// The trait an operator calls on values that are not built-in numbers or collections.
    pub fn of_operator(op: BinaryOp) -> Option<Self> {
        Some(match op {
            BinaryOp::Add => Self::Add,
            BinaryOp::Sub => Self::Sub,
            BinaryOp::Mul => Self::Mul,
            BinaryOp::Div => Self::Div,
            BinaryOp::Rem => Self::Rem,
            BinaryOp::Concat => Self::Concat,
            _ => return None,
        })
    }
}

/// A user-defined trait (spec section 10.3), or a prelude trait.
#[derive(Clone, Debug)]
pub struct TraitDef {
    /// The trait's name.
    pub name: Spanned<String>,
    /// The module that declares it.
    pub module: ModuleId,
    /// For a prelude trait, which one; it has no source location.
    pub prelude: Option<PreludeTrait>,
    /// The traits listed in `impl=`: a type implementing this trait must implement them too.
    pub supertraits: Vec<Spanned<Bound>>,
    /// The trait's functions, by name, in declaration order. Each has `Self` as its first type
    /// parameter; required functions have no body.
    pub functions: Vec<(String, FnId)>,
}

impl TraitDef {
    /// The function with the given name.
    pub fn function(&self, name: &str) -> Option<FnId> {
        self.functions
            .iter()
            .find(|(n, _)| n == name)
            .map(|&(_, id)| id)
    }
}

/// The built-in traits a type derives, each with where it is listed.
#[derive(Clone, Debug, Default)]
pub struct Derives {
    /// `Copy`: values are copied instead of moved.
    pub copy: Option<Span>,
    /// `Clone`: `[$value->clone]` makes a deep copy.
    pub clone: Option<Span>,
    /// `Eq`: `=` and `!=` compare fields.
    pub eq: Option<Span>,
    /// `Ord`: `<`, `<=`, `>` and `>=` compare fields in order.
    pub ord: Option<Span>,
    /// `Hash`: values can be keys of maps and elements of sets.
    pub hash: Option<Span>,
    /// `Display`: values can be printed, as `Name{field=value; ...}` or
    /// `Name->variant{field=value; ...}`.
    pub display: Option<Span>,
    /// `Default`: `[:default]` makes a value of each field's default.
    pub default: Option<Span>,
}

/// What kind of binding a local variable is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalKind {
    /// A function parameter.
    Param(Convention),
    /// `:local`, which can be reassigned.
    Var,
    /// `:const` inside a function, which cannot be reassigned.
    Const,
    /// The variable of a `:for` loop.
    LoopVar,
    /// A name bound by a pattern of a `:match` arm.
    Binding,
    /// An element (or map value) bound by `:foreach`; `mutable` for `:foreach mut`.
    Element {
        /// Whether the element may be modified.
        mutable: bool,
    },
    /// The error caught by `:onerror`.
    Caught,
    /// A variable of an enclosing function, captured by a closure.
    Captured,
}

impl LocalKind {
    /// Returns true if the variable can be the target of `:set`.
    pub fn is_mutable(self) -> bool {
        matches!(
            self,
            Self::Var
                | Self::Param(Convention::Mut | Convention::Owned)
                | Self::Element { mutable: true }
        )
    }
}

/// A local variable or parameter.
#[derive(Clone, Debug)]
pub struct Local {
    /// The variable's name.
    pub name: Spanned<String>,
    /// What kind of binding it is.
    pub kind: LocalKind,
    /// The declared type, if any.
    pub ty: Option<Spanned<Ty>>,
}

/// A sequence of statements.
#[derive(Clone, Debug, Default)]
pub struct Block {
    /// The statements, in order.
    pub stmts: Vec<StmtId>,
    /// The span of the block, used for diagnostics about its end.
    pub span: Option<Span>,
}

/// A statement.
#[derive(Clone, Debug)]
pub enum Stmt {
    /// A command used as a statement.
    Expr(ExprId),
    /// `:local` or `:const` inside a function.
    Let {
        /// The declared variable.
        local: LocalId,
        /// The initial value.
        init: Option<ExprId>,
    },
    /// `:set target value`
    Set {
        /// What is assigned.
        target: Place,
        /// The new value.
        value: ExprId,
    },
    /// `:if (cond) do={...} else={...}`
    If {
        /// The condition.
        cond: ExprId,
        /// The `do=` block.
        then_block: Block,
        /// The `else=` branch.
        else_branch: Option<ElseBranch>,
    },
    /// `:while (cond) do={...}`
    While {
        /// The condition.
        cond: ExprId,
        /// The loop body.
        body: Block,
    },
    /// `:do {...} while=(cond)`
    DoWhile {
        /// The loop body.
        body: Block,
        /// The condition.
        cond: ExprId,
    },
    /// `:for var from=... to=.../until=... step=... do={...}`
    For {
        /// The loop variable.
        var: LocalId,
        /// The first value.
        from: ExprId,
        /// The bound, absent only after an error.
        end: Option<(ForEnd, ExprId)>,
        /// The increment.
        step: Option<ExprId>,
        /// The loop body.
        body: Block,
    },
    /// `{ ... }`
    Block(Block),
    /// `:foreach x in=... do={...}`, `:foreach i,x ...` or `:foreach k,v ...`
    Foreach {
        /// The index of a list element, or the key of a map entry.
        key: Option<LocalId>,
        /// The element, or the value of a map entry.
        value: LocalId,
        /// The collection iterated over.
        collection: ExprId,
        /// The loop body.
        body: Block,
    },
    /// `:match scrutinee { arms }`
    Match {
        /// The value matched on.
        scrutinee: ExprId,
        /// The arms, in order.
        arms: Vec<MatchArm>,
    },
    /// `:onerror e in={...} do={...}` (spec section 9.2)
    OnError {
        /// The error, in scope in the handler.
        error: LocalId,
        /// The block whose errors are caught.
        body: Block,
        /// The block run with the error.
        handler: Block,
    },
}

/// An arm of a `:match`.
#[derive(Clone, Debug)]
pub struct MatchArm {
    /// The pattern.
    pub pat: PatId,
    /// The guard of `if=(guard)`.
    pub guard: Option<ExprId>,
    /// The body.
    pub body: Block,
}

/// A pattern.
#[derive(Clone, Debug, PartialEq)]
pub enum Pat {
    /// An invalid pattern, already reported, with the sub-patterns written in it (whose
    /// names are still declared). Matches anything.
    Invalid(Vec<PatId>),
    /// `_`
    Wildcard,
    /// A name, binding the value matched.
    Binding(LocalId),
    /// A literal: the value must be equal.
    Literal(Literal),
    /// A string literal: the value must be equal.
    String(String),
    /// `Enum->variant p1 p2 ...`
    Variant {
        /// The enum.
        ty: TypeId,
        /// The variant's index.
        variant: usize,
        /// A pattern for each field, in order.
        fields: Vec<PatId>,
        /// The enum's type arguments, if written.
        type_args: TypeArgs,
    },
    /// `some p`
    Some(PatId),
    /// `none`
    None,
}

/// Whether the bound of a `:for` loop is included.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ForEnd {
    /// `to=`: inclusive.
    To,
    /// `until=`: exclusive.
    Until,
}

/// The `else=` branch of an `:if`.
#[derive(Clone, Debug)]
pub enum ElseBranch {
    /// `else={...}`
    Block(Block),
    /// `else=:if ...`, an [`Stmt::If`].
    If(StmtId),
}

/// The target of `:set`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Place {
    /// A local variable.
    Local(LocalId),
    /// A module-level variable.
    Global(GlobalId),
    /// A part of a variable: a field, the value of a box, or an element of a list or map.
    /// An [`Expr::Field`] or [`Expr::Index`], whose innermost base is a variable.
    Part(ExprId),
    /// An invalid target, already reported.
    Error,
}

/// An expression.
#[derive(Clone, Debug, PartialEq)]
pub enum Expr {
    /// A missing or invalid expression, already reported.
    Missing,
    /// A literal.
    Literal(Literal),
    /// A string literal, possibly with interpolations.
    String(Vec<StringPart>),
    /// A local variable or parameter.
    Local(LocalId),
    /// A module-level constant.
    Const(ConstId),
    /// A module-level variable.
    Global(GlobalId),
    /// A call.
    Call {
        /// What is called.
        callee: Callee,
        /// The arguments, in source order.
        args: Vec<CallArg>,
        /// The type arguments of the owner type of an associated function, as in
        /// `[Pair<i64, String>->new ...]`.
        owner_args: TypeArgs,
        /// The type arguments of the function, as in `[:max<f64> 1.0 2.0]`.
        fn_args: TypeArgs,
        /// For a function of a trait called on a type, as in `[Square->make]` or `[T->make]`:
        /// that type, with its own type parameters as arguments (replaced by `owner_args`
        /// when they are written).
        self_ty: Option<Ty>,
    },
    /// A struct literal: `Point{x=1.0; y=2.0}`. The values are in field order; `None` takes
    /// the field's default value.
    StructLit {
        /// The struct.
        strukt: TypeId,
        /// The value of each field.
        fields: Vec<Option<ExprId>>,
        /// The struct's type arguments, if written: `Pair<i64, String>{...}`.
        type_args: TypeArgs,
    },
    /// A field of a value: `$p->x`, or the value in a box: `$b->value`. The field is
    /// resolved by the type checker, from the base's type.
    Field {
        /// The value whose field is read.
        base: ExprId,
        /// The field's name.
        field: Spanned<String>,
    },
    /// A method call: `[$value->method args]`. The method is resolved by the type checker,
    /// from the receiver's type.
    MethodCall {
        /// The value the method is called on.
        receiver: ExprId,
        /// The method's name.
        method: Spanned<String>,
        /// The arguments, in source order.
        args: Vec<CallArg>,
        /// Explicit type arguments of the method, as in `[$a->map<f64> ...]`.
        type_args: TypeArgs,
    },
    /// An enum value: `Token->plus` or `[Token->number 3.5]`.
    Variant {
        /// The enum.
        ty: TypeId,
        /// The variant's index.
        variant: usize,
        /// The values of the fields, as call arguments.
        args: Vec<CallArg>,
        /// The enum's type arguments, if written: `[Tree<i64>->leaf 1]`.
        type_args: TypeArgs,
    },
    /// `[some value]`
    Some(ExprId),
    /// `none`
    None,
    /// `[Box->new value]`
    BoxNew(ExprId),
    /// `[:fn params -> Ret do={...}]`: a closure, whose body is the given function.
    Closure(FnId),
    /// `$name` for a function `name`: the function as a value.
    FnRef(FnId),
    /// `[$f args]`: a call of a function value, with positional arguments.
    CallValue {
        /// The function value.
        callee: ExprId,
        /// The arguments, in order.
        args: Vec<ExprId>,
    },
    /// `:error value` or `:error value source=$cause`: raises an error, from a message or an
    /// `Error` (spec section 9.1).
    Raise {
        /// The message, or the error.
        value: ExprId,
        /// The error that caused this one.
        source: Option<ExprId>,
    },
    /// A collection literal: `{1; 2}`, `{"a"=1}`, `List<i64>{}`. Whether it is a list, a set
    /// or a map is decided by the type checker, from the written type, the expected type,
    /// or the elements.
    Collection {
        /// The type written before the brace.
        declared: Option<Spanned<Ty>>,
        /// The elements, in order.
        elements: Vec<Element>,
    },
    /// An element of a list or a value of a map: `$xs->0`, `$m->"key"`, `$xs->$i`.
    Index {
        /// The collection.
        base: ExprId,
        /// The index or key.
        index: ExprId,
    },
    /// `-x`, `!x`, `~x`
    Unary {
        /// The operator.
        op: UnaryOp,
        /// The operand.
        operand: ExprId,
    },
    /// `lhs op rhs`
    Binary {
        /// The operator.
        op: BinaryOp,
        /// Where the operator was written.
        op_span: Span,
        /// The left operand.
        lhs: ExprId,
        /// The right operand.
        rhs: ExprId,
    },
    /// `expr as Type`
    Cast {
        /// The converted expression.
        expr: ExprId,
        /// The target type.
        ty: Spanned<Ty>,
    },
    /// `:return` or `:return value`
    Return(Option<ExprId>),
    /// `:break`
    Break,
    /// `:continue`
    Continue,
}

impl Expr {
    /// The expressions directly inside this one, in evaluation order.
    pub fn children(&self) -> Vec<ExprId> {
        match self {
            Self::Missing
            | Self::Literal(_)
            | Self::Local(_)
            | Self::Const(_)
            | Self::Global(_)
            | Self::Break
            | Self::Continue
            | Self::Closure(_)
            | Self::FnRef(_)
            | Self::None => Vec::new(),
            Self::String(parts) => parts
                .iter()
                .filter_map(|part| match part {
                    StringPart::Expr(expr) => Some(*expr),
                    StringPart::Text(_) => None,
                })
                .collect(),
            Self::Call { args, .. } | Self::Variant { args, .. } => {
                args.iter().map(|arg| arg.value).collect()
            }
            Self::MethodCall { receiver, args, .. } => std::iter::once(*receiver)
                .chain(args.iter().map(|arg| arg.value))
                .collect(),
            Self::StructLit { fields, .. } => fields.iter().flatten().copied().collect(),
            Self::Field { base, .. } => vec![*base],
            Self::Unary { operand, .. } => vec![*operand],
            Self::Binary { lhs, rhs, .. } => vec![*lhs, *rhs],
            Self::Cast { expr, .. } => vec![*expr],
            Self::Return(value) => value.iter().copied().collect(),
            Self::Some(value) | Self::BoxNew(value) => vec![*value],
            Self::Raise { value, source } => std::iter::once(*value).chain(*source).collect(),
            Self::CallValue { callee, args } => std::iter::once(*callee)
                .chain(args.iter().copied())
                .collect(),
            Self::Collection { elements, .. } => elements
                .iter()
                .flat_map(|element| match *element {
                    Element::Value(value) => vec![value],
                    Element::Entry(key, value) => vec![key, value],
                })
                .collect(),
            Self::Index { base, index } => vec![*base, *index],
        }
    }
}

/// An element of a collection literal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Element {
    /// A value, of a list or set.
    Value(ExprId),
    /// `key=value`, of a map.
    Entry(ExprId, ExprId),
}

/// A literal value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Literal {
    /// An integer. The sign is kept separately so that the range of the final type can be
    /// checked once the type is known.
    Int {
        /// The magnitude.
        value: u64,
        /// Whether a `-` was written.
        negative: bool,
    },
    /// A float (already negated if a `-` was written).
    Float(f64),
    /// `true` or `false`.
    Bool(bool),
    /// A character.
    Char(char),
    /// A duration in nanoseconds (already negated if a `-` was written).
    Duration(i64),
}

/// A piece of a string literal.
#[derive(Clone, Debug, PartialEq)]
pub enum StringPart {
    /// Literal text, with escapes resolved.
    Text(String),
    /// An interpolated value.
    Expr(ExprId),
}

/// What a call invokes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Callee {
    /// A user-defined function.
    Fn(FnId),
    /// A built-in command.
    Builtin(Builtin),
    /// An unknown or unsupported callee, already reported.
    Error,
}

/// A call argument.
#[derive(Clone, Debug, PartialEq)]
pub struct CallArg {
    /// The name, for `name=value`.
    pub name: Option<Spanned<String>>,
    /// The value.
    pub value: ExprId,
}

/// Built-in commands that are ordinary calls (spec section 8.4). `:return`, `:break` and
/// `:continue` are expressions of their own.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Builtin {
    /// `:put value`
    Put,
    /// `:len value`
    Len,
    /// `:tostr value`
    ToStr,
    /// `:typeof value`
    TypeOf,
    /// `:assert (cond) message?`
    Assert,
    /// `:panic message`
    Panic,
    /// `:nothing`
    Nothing,
    /// `:default`, or `:default<T>`: the default value of a type.
    Default,
}

impl Builtin {
    /// The built-in command with the given name.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "put" => Self::Put,
            "len" => Self::Len,
            "tostr" => Self::ToStr,
            "typeof" => Self::TypeOf,
            "assert" => Self::Assert,
            "panic" => Self::Panic,
            "nothing" => Self::Nothing,
            "default" => Self::Default,
            _ => return None,
        })
    }

    /// The command's name, without `:`.
    pub fn name(self) -> &'static str {
        match self {
            Self::Put => "put",
            Self::Len => "len",
            Self::ToStr => "tostr",
            Self::TypeOf => "typeof",
            Self::Assert => "assert",
            Self::Panic => "panic",
            Self::Nothing => "nothing",
            Self::Default => "default",
        }
    }
}

/// Command names reserved by the language (spec section 3.6). They cannot name functions.
pub const RESERVED_COMMANDS: &[&str] = &[
    "local", "const", "global", "set", "if", "do", "while", "for", "foreach", "match", "break",
    "continue", "return", "error", "onerror", "panic", "fn", "struct", "enum", "trait", "impl",
    "use", "extern", "unsafe", "test", "put", "len", "typeof", "tostr", "assert", "nothing",
    "default",
];
