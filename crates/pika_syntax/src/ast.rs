//! A typed view over the concrete syntax tree.
//!
//! Each AST type wraps a [`SyntaxNode`] of one kind and offers accessors for its parts. The
//! wrappers never fail on malformed input: a missing part is returned as `None`, because the
//! tree of a file with syntax errors is still a valid tree.

use pika_diagnostics::Span;
use rowan::TextRange;

use crate::syntax_kind::{SyntaxKind, SyntaxNode, SyntaxToken};

std::thread_local! {
    /// The offset of the file whose tree is being read, set by [`with_span_base`].
    static SPAN_BASE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// Runs `f` with the spans of syntax trees taken to be in a file that starts at offset
/// `base` of a [`pika_diagnostics::SourceMap`]. Outside of it, spans are from offset 0.
pub fn with_span_base<R>(base: u32, f: impl FnOnce() -> R) -> R {
    let outer = SPAN_BASE.replace(base);
    let result = f();
    SPAN_BASE.set(outer);
    result
}

/// Converts a `rowan` text range into a [`Span`], in the file set by [`with_span_base`].
pub fn span_of(range: TextRange) -> Span {
    let base = SPAN_BASE.get();
    Span::new(
        base + u32::from(range.start()),
        base + u32::from(range.end()),
    )
}

/// A typed wrapper around a syntax node of a particular kind.
pub trait AstNode: Sized {
    /// Returns true if nodes of `kind` can be wrapped by this type.
    fn can_cast(kind: SyntaxKind) -> bool;
    /// Wraps `node` if it has the right kind.
    fn cast(node: SyntaxNode) -> Option<Self>;
    /// The wrapped node.
    fn syntax(&self) -> &SyntaxNode;

    /// The source range of the node, excluding surrounding whitespace and comments.
    fn span(&self) -> Span {
        span_of(trimmed_range(self.syntax()))
    }
}

/// The range of `node` without leading or trailing trivia and line breaks.
fn trimmed_range(node: &SyntaxNode) -> TextRange {
    let significant = |t: &SyntaxToken| !t.kind().is_trivia() && t.kind() != SyntaxKind::Newline;
    let mut tokens = node
        .descendants_with_tokens()
        .filter_map(rowan::NodeOrToken::into_token)
        .filter(significant);
    let Some(first) = tokens.next() else {
        return node.text_range();
    };
    let last = tokens.last().unwrap_or_else(|| first.clone());
    TextRange::new(first.text_range().start(), last.text_range().end())
}

macro_rules! ast_node {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Debug, PartialEq, Eq, Hash)]
        pub struct $name(SyntaxNode);

        impl AstNode for $name {
            fn can_cast(kind: SyntaxKind) -> bool {
                kind == SyntaxKind::$name
            }
            fn cast(node: SyntaxNode) -> Option<Self> {
                Self::can_cast(node.kind()).then(|| Self(node))
            }
            fn syntax(&self) -> &SyntaxNode {
                &self.0
            }
        }
    };
}

fn child<N: AstNode>(node: &SyntaxNode) -> Option<N> {
    node.children().find_map(N::cast)
}

fn children<N: AstNode>(node: &SyntaxNode) -> impl Iterator<Item = N> {
    node.children().filter_map(N::cast)
}

fn token(node: &SyntaxNode, kind: SyntaxKind) -> Option<SyntaxToken> {
    node.children_with_tokens()
        .filter_map(rowan::NodeOrToken::into_token)
        .find(|t| t.kind() == kind)
}

/// Returns true if `node` has a direct `Ident` token child with the given text, such as the
/// contextual keywords `raises`, `mut` and `owned`.
fn has_contextual(node: &SyntaxNode, text: &str) -> bool {
    node.children_with_tokens()
        .filter_map(rowan::NodeOrToken::into_token)
        .any(|t| t.kind() == SyntaxKind::Ident && t.text() == text)
}

/// The named argument `name=...` among the direct children of `node`.
fn named_arg(node: &SyntaxNode, name: &str) -> Option<NamedArg> {
    children::<NamedArg>(node).find(|arg| arg.name_text().as_deref() == Some(name))
}

// ----- Files and statements ----------------------------------------------------------------

ast_node!(
    /// A whole source file.
    SourceFile
);

impl SourceFile {
    /// The top-level statements and declarations, in order.
    pub fn stmts(&self) -> impl Iterator<Item = Stmt> {
        self.0.children().filter_map(Stmt::cast)
    }
}

ast_node!(
    /// `{ statements }`
    Block
);

impl Block {
    /// The statements of the block, in order.
    pub fn stmts(&self) -> impl Iterator<Item = Stmt> {
        self.0.children().filter_map(Stmt::cast)
    }

    /// The span of the closing `}`, if present.
    pub fn closing_brace_span(&self) -> Option<Span> {
        token(&self.0, SyntaxKind::RBrace).map(|t| span_of(t.text_range()))
    }
}

/// A statement or declaration.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs, reason = "each variant wraps the node of the same name")]
pub enum Stmt {
    Call(Call),
    LocalDecl(LocalDecl),
    ConstDecl(ConstDecl),
    GlobalDecl(GlobalDecl),
    SetStmt(SetStmt),
    IfStmt(IfStmt),
    WhileStmt(WhileStmt),
    DoWhileStmt(DoWhileStmt),
    ForStmt(ForStmt),
    ForeachStmt(ForeachStmt),
    MatchStmt(MatchStmt),
    OnErrorStmt(OnErrorStmt),
    UnsafeBlock(UnsafeBlock),
    Block(Block),
    FnDecl(FnDecl),
    StructDecl(StructDecl),
    EnumDecl(EnumDecl),
    TraitDecl(TraitDecl),
    ImplDecl(ImplDecl),
    UseDecl(UseDecl),
    ExternDecl(ExternDecl),
    TestDecl(TestDecl),
}

impl Stmt {
    /// Wraps `node` if it is a statement or declaration.
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        Some(match node.kind() {
            SyntaxKind::Call => Self::Call(Call(node)),
            SyntaxKind::LocalDecl => Self::LocalDecl(LocalDecl(node)),
            SyntaxKind::ConstDecl => Self::ConstDecl(ConstDecl(node)),
            SyntaxKind::GlobalDecl => Self::GlobalDecl(GlobalDecl(node)),
            SyntaxKind::SetStmt => Self::SetStmt(SetStmt(node)),
            SyntaxKind::IfStmt => Self::IfStmt(IfStmt(node)),
            SyntaxKind::WhileStmt => Self::WhileStmt(WhileStmt(node)),
            SyntaxKind::DoWhileStmt => Self::DoWhileStmt(DoWhileStmt(node)),
            SyntaxKind::ForStmt => Self::ForStmt(ForStmt(node)),
            SyntaxKind::ForeachStmt => Self::ForeachStmt(ForeachStmt(node)),
            SyntaxKind::MatchStmt => Self::MatchStmt(MatchStmt(node)),
            SyntaxKind::OnErrorStmt => Self::OnErrorStmt(OnErrorStmt(node)),
            SyntaxKind::UnsafeBlock => Self::UnsafeBlock(UnsafeBlock(node)),
            SyntaxKind::Block => Self::Block(Block(node)),
            SyntaxKind::FnDecl => Self::FnDecl(FnDecl(node)),
            SyntaxKind::StructDecl => Self::StructDecl(StructDecl(node)),
            SyntaxKind::EnumDecl => Self::EnumDecl(EnumDecl(node)),
            SyntaxKind::TraitDecl => Self::TraitDecl(TraitDecl(node)),
            SyntaxKind::ImplDecl => Self::ImplDecl(ImplDecl(node)),
            SyntaxKind::UseDecl => Self::UseDecl(UseDecl(node)),
            SyntaxKind::ExternDecl => Self::ExternDecl(ExternDecl(node)),
            SyntaxKind::TestDecl => Self::TestDecl(TestDecl(node)),
            _ => return None,
        })
    }

    /// The wrapped node.
    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::Call(n) => n.syntax(),
            Self::LocalDecl(n) => n.syntax(),
            Self::ConstDecl(n) => n.syntax(),
            Self::GlobalDecl(n) => n.syntax(),
            Self::SetStmt(n) => n.syntax(),
            Self::IfStmt(n) => n.syntax(),
            Self::WhileStmt(n) => n.syntax(),
            Self::DoWhileStmt(n) => n.syntax(),
            Self::ForStmt(n) => n.syntax(),
            Self::ForeachStmt(n) => n.syntax(),
            Self::MatchStmt(n) => n.syntax(),
            Self::OnErrorStmt(n) => n.syntax(),
            Self::UnsafeBlock(n) => n.syntax(),
            Self::Block(n) => n.syntax(),
            Self::FnDecl(n) => n.syntax(),
            Self::StructDecl(n) => n.syntax(),
            Self::EnumDecl(n) => n.syntax(),
            Self::TraitDecl(n) => n.syntax(),
            Self::ImplDecl(n) => n.syntax(),
            Self::UseDecl(n) => n.syntax(),
            Self::ExternDecl(n) => n.syntax(),
            Self::TestDecl(n) => n.syntax(),
        }
    }

    /// The source range of the statement.
    pub fn span(&self) -> Span {
        span_of(trimmed_range(self.syntax()))
    }
}

/// The `:name` that starts a built-in form, such as `:if` or `:struct`.
fn form_keyword_span(node: &SyntaxNode) -> Span {
    let mut tokens = node
        .children_with_tokens()
        .filter_map(rowan::NodeOrToken::into_token)
        .filter(|t| !t.kind().is_trivia());
    match (tokens.next(), tokens.next()) {
        (Some(colon), Some(name)) => span_of(TextRange::new(
            colon.text_range().start(),
            name.text_range().end(),
        )),
        _ => span_of(node.text_range()),
    }
}

macro_rules! form_keyword {
    ($($name:ident),*) => {
        $(impl $name {
            /// The span of the `:name` keyword that starts this form.
            pub fn keyword_span(&self) -> Span {
                form_keyword_span(&self.0)
            }
        })*
    };
}

ast_node!(
    /// A call: `:name args`, `/path args`, `$value args` or `Type->member args`.
    Call
);

/// What a call invokes.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Callee {
    /// `:name` or `:name<T>`.
    Command(CommandName),
    /// Any other head: a path, a variable, a member access.
    Expr(Expr),
    /// `some`, constructing an option.
    Some(SomeHead),
}

impl Call {
    /// The head of the call.
    pub fn callee(&self) -> Option<Callee> {
        if let Some(name) = child::<CommandName>(&self.0) {
            return Some(Callee::Command(name));
        }
        if let Some(head) = child::<SomeHead>(&self.0) {
            return Some(Callee::Some(head));
        }
        self.0.children().find_map(Expr::cast).map(Callee::Expr)
    }

    /// The arguments, in source order.
    pub fn args(&self) -> Vec<Arg> {
        child::<ArgList>(&self.0)
            .map(|list| list.0.children().filter_map(Arg::cast).collect())
            .unwrap_or_default()
    }

    /// Returns true if the head is not a valid callee (for example `put "hi"`).
    pub fn has_error_head(&self) -> bool {
        self.0
            .children()
            .next()
            .is_some_and(|n| n.kind() == SyntaxKind::Error)
    }
}

ast_node!(
    /// `:name` or `:name<T>` at the head of a call.
    CommandName
);
ast_node!(
    /// The `some` keyword at the head of a call: `[some $x]`.
    SomeHead
);

impl CommandName {
    /// The command's name without the colon.
    pub fn name(&self) -> Option<SyntaxToken> {
        token(&self.0, SyntaxKind::Ident)
    }

    /// Explicit generic arguments, as in `:parse<i64>`.
    pub fn generic_args(&self) -> Option<GenericArgList> {
        child(&self.0)
    }
}

ast_node!(
    /// The arguments of a call.
    ArgList
);

/// A call argument.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Arg {
    /// A positional argument.
    Positional(Expr),
    /// `name=value`.
    Named(NamedArg),
}

impl Arg {
    fn cast(node: SyntaxNode) -> Option<Self> {
        if let Some(named) = NamedArg::cast(node.clone()) {
            return Some(Self::Named(named));
        }
        Expr::cast(node).map(Self::Positional)
    }
}

ast_node!(
    /// `name=value`, in a call or in a built-in form.
    NamedArg
);

impl NamedArg {
    /// The argument's name.
    pub fn name(&self) -> Option<NameRef> {
        child(&self.0)
    }

    /// The argument's name as text.
    pub fn name_text(&self) -> Option<String> {
        self.name().map(|n| n.text())
    }

    /// The value, if it is an expression.
    pub fn expr(&self) -> Option<Expr> {
        self.0.children().skip(1).find_map(Expr::cast)
    }

    /// The value, if it is a block (as in `do={...}`).
    pub fn block(&self) -> Option<Block> {
        child(&self.0)
    }

    /// The value, if it is an `:if` (as in `else=:if ...`).
    pub fn if_stmt(&self) -> Option<IfStmt> {
        child(&self.0)
    }
}

ast_node!(
    /// `:local name:Type value`
    LocalDecl
);
ast_node!(
    /// `:const name:Type value`
    ConstDecl
);
ast_node!(
    /// `:global NAME:Type value`
    GlobalDecl
);

macro_rules! var_decl_accessors {
    ($($name:ident),*) => {
        $(impl $name {
            /// The declared name.
            pub fn name(&self) -> Option<Name> {
                child(&self.0)
            }
            /// The type annotation, if any.
            pub fn ty(&self) -> Option<Type> {
                child::<TypeAnnotation>(&self.0).and_then(|a| a.ty())
            }
            /// The initial value, if any.
            pub fn value(&self) -> Option<Expr> {
                self.0.children().find_map(Expr::cast)
            }
        })*
    };
}
var_decl_accessors!(LocalDecl, ConstDecl, GlobalDecl);

ast_node!(
    /// `:set target value`
    SetStmt
);

/// The target of `:set`.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum SetTarget {
    /// `:set name ...`
    Name(NameRef),
    /// `:set (place) ...`, or an invalid target already reported by the parser.
    Expr(Expr),
}

impl SetStmt {
    /// What is assigned to.
    pub fn target(&self) -> Option<SetTarget> {
        let first = self.0.children().next()?;
        if let Some(name) = NameRef::cast(first.clone()) {
            return Some(SetTarget::Name(name));
        }
        Expr::cast(first).map(SetTarget::Expr)
    }

    /// The assigned value.
    pub fn value(&self) -> Option<Expr> {
        // The first child is the target (a name or an expression); the value follows it.
        self.0.children().skip(1).find_map(Expr::cast)
    }
}

ast_node!(
    /// `:if (cond) do={...} else={...}`
    IfStmt
);

impl IfStmt {
    /// The condition.
    pub fn condition(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
    /// The `do=` block.
    pub fn then_block(&self) -> Option<Block> {
        named_arg(&self.0, "do").and_then(|a| a.block())
    }
    /// The `else=` argument.
    pub fn else_arg(&self) -> Option<NamedArg> {
        named_arg(&self.0, "else")
    }
}

ast_node!(
    /// `:while (cond) do={...}`
    WhileStmt
);

impl WhileStmt {
    /// The condition.
    pub fn condition(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
    /// The loop body.
    pub fn body(&self) -> Option<Block> {
        named_arg(&self.0, "do").and_then(|a| a.block())
    }
}

ast_node!(
    /// `:do {...} while=(cond)`
    DoWhileStmt
);

impl DoWhileStmt {
    /// The loop body.
    pub fn body(&self) -> Option<Block> {
        child(&self.0)
    }
    /// The condition.
    pub fn condition(&self) -> Option<Expr> {
        named_arg(&self.0, "while").and_then(|a| a.expr())
    }
}

ast_node!(
    /// `:for i from=a to=b step=c do={...}`
    ForStmt
);

impl ForStmt {
    /// The loop variable.
    pub fn var(&self) -> Option<Name> {
        child(&self.0)
    }
    /// The named argument `name=` (`from`, `to`, `until`, `step`, `do`).
    pub fn arg(&self, name: &str) -> Option<NamedArg> {
        named_arg(&self.0, name)
    }
    /// The loop body.
    pub fn body(&self) -> Option<Block> {
        self.arg("do").and_then(|a| a.block())
    }
}

ast_node!(
    /// `:foreach x in=... do={...}`
    ForeachStmt
);

impl ForeachStmt {
    /// Returns true for `:foreach mut x ...`, which may modify the elements.
    pub fn is_mut(&self) -> bool {
        has_contextual(&self.0, "mut")
    }
    /// The loop variables: the element, or the index (or key) and the element (or value).
    pub fn names(&self) -> Vec<Name> {
        children(&self.0).collect()
    }
    /// The collection iterated over.
    pub fn collection(&self) -> Option<Expr> {
        named_arg(&self.0, "in").and_then(|arg| arg.expr())
    }
    /// The loop body.
    pub fn body(&self) -> Option<Block> {
        named_arg(&self.0, "do").and_then(|arg| arg.block())
    }
}
ast_node!(
    /// `:match $value { arms }`
    MatchStmt
);

impl MatchStmt {
    /// The value matched on.
    pub fn scrutinee(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
    /// The arms, in order.
    pub fn arms(&self) -> Vec<MatchArm> {
        child::<MatchArmList>(&self.0)
            .map(|list| children(&list.0).collect())
            .unwrap_or_default()
    }
    /// The span of the opening brace of the arms, or of the whole statement.
    pub fn arms_span(&self) -> Span {
        child::<MatchArmList>(&self.0).map_or_else(|| self.span(), |list| list.span())
    }
}

ast_node!(
    /// The `{ arms }` of a `:match`.
    MatchArmList
);
ast_node!(
    /// `Pattern if=(guard) do={...}`
    MatchArm
);

impl MatchArm {
    /// The pattern.
    pub fn pattern(&self) -> Option<Pattern> {
        self.0.children().find_map(Pattern::cast)
    }
    /// The guard of `if=(guard)`.
    pub fn guard(&self) -> Option<Expr> {
        named_arg(&self.0, "if").and_then(|arg| arg.expr())
    }
    /// The body of `do={...}`.
    pub fn body(&self) -> Option<Block> {
        named_arg(&self.0, "do").and_then(|arg| arg.block())
    }
}

/// A pattern of a `:match` arm.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Pattern {
    /// `_`
    Wildcard(WildcardPat),
    /// `name`
    Binding(BindingPat),
    /// A literal: `1`, `-2`, `'c'`, `"text"`, `true`.
    Literal(LiteralPat),
    /// `Enum->variant p1 p2 ...`
    Variant(VariantPat),
    /// `some p`
    Some(SomePat),
    /// `none`
    None(NonePat),
    /// `(p)`
    Paren(ParenPat),
}

impl Pattern {
    fn cast(node: SyntaxNode) -> Option<Self> {
        Some(match node.kind() {
            SyntaxKind::WildcardPat => Self::Wildcard(WildcardPat(node)),
            SyntaxKind::BindingPat => Self::Binding(BindingPat(node)),
            SyntaxKind::LiteralPat => Self::Literal(LiteralPat(node)),
            SyntaxKind::VariantPat => Self::Variant(VariantPat(node)),
            SyntaxKind::SomePat => Self::Some(SomePat(node)),
            SyntaxKind::NonePat => Self::None(NonePat(node)),
            SyntaxKind::ParenPat => Self::Paren(ParenPat(node)),
            _ => return None,
        })
    }

    /// The source range of the pattern.
    pub fn span(&self) -> Span {
        match self {
            Self::Wildcard(p) => p.span(),
            Self::Binding(p) => p.span(),
            Self::Literal(p) => p.span(),
            Self::Variant(p) => p.span(),
            Self::Some(p) => p.span(),
            Self::None(p) => p.span(),
            Self::Paren(p) => p.span(),
        }
    }
}

ast_node!(
    /// `_`
    WildcardPat
);
ast_node!(
    /// `name`
    BindingPat
);

impl BindingPat {
    /// The bound name.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
}

ast_node!(
    /// A literal pattern.
    LiteralPat
);

impl LiteralPat {
    /// The literal, unless it is a string.
    pub fn literal(&self) -> Option<(LiteralKind, bool)> {
        let negative = token(&self.0, SyntaxKind::Minus).is_some();
        literal_kind(&self.0).map(|kind| (kind, negative))
    }
    /// The string, for a string pattern.
    pub fn string(&self) -> Option<StringLit> {
        child(&self.0)
    }
}

ast_node!(
    /// `Enum->variant p1 p2 ...`
    VariantPat
);

impl VariantPat {
    /// The enum's type.
    pub fn ty(&self) -> Option<PathType> {
        child(&self.0)
    }
    /// The variant's name.
    pub fn variant(&self) -> Option<NameRef> {
        child(&self.0)
    }
    /// The sub-patterns of the variant's fields.
    pub fn fields(&self) -> Vec<Pattern> {
        self.0.children().filter_map(Pattern::cast).collect()
    }
}

ast_node!(
    /// `some p`
    SomePat
);

impl SomePat {
    /// The pattern of the value.
    pub fn pattern(&self) -> Option<Pattern> {
        self.0.children().find_map(Pattern::cast)
    }
}

ast_node!(
    /// `none`
    NonePat
);
ast_node!(
    /// `(p)`
    ParenPat
);

impl ParenPat {
    /// The pattern inside.
    pub fn pattern(&self) -> Option<Pattern> {
        self.0.children().find_map(Pattern::cast)
    }
}
ast_node!(
    /// `:onerror e in={...} do={...}`
    OnErrorStmt
);

impl OnErrorStmt {
    /// The name of the error in the handler.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    /// The block whose errors are caught.
    pub fn body(&self) -> Option<Block> {
        named_arg(&self.0, "in").and_then(|arg| arg.block())
    }
    /// The block run when the body raises an error.
    pub fn handler(&self) -> Option<Block> {
        named_arg(&self.0, "do").and_then(|arg| arg.block())
    }
}
ast_node!(
    /// `:unsafe {...}`
    UnsafeBlock
);

/// Every named argument of a form, for checking duplicates.
pub fn named_args(node: &SyntaxNode) -> impl Iterator<Item = NamedArg> {
    children::<NamedArg>(node)
}

// ----- Declarations ------------------------------------------------------------------------

ast_node!(
    /// `:fn name params -> Ret raises do={...}`
    FnDecl
);

impl FnDecl {
    /// The function's name; absent for anonymous functions.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    /// Generic parameters, if any.
    pub fn generic_params(&self) -> Option<GenericParamList> {
        child(&self.0)
    }
    /// The parameters.
    pub fn params(&self) -> Vec<Param> {
        child::<ParamList>(&self.0)
            .map(|list| children(&list.0).collect())
            .unwrap_or_default()
    }
    /// The return type, if declared.
    pub fn ret_type(&self) -> Option<Type> {
        child::<RetType>(&self.0).and_then(|r| r.0.children().find_map(Type::cast))
    }
    /// The span of the return type annotation, `-> Type`.
    pub fn ret_type_span(&self) -> Option<Span> {
        child::<RetType>(&self.0).map(|r| r.span())
    }
    /// Returns true if the function is declared `raises`.
    pub fn is_raises(&self) -> bool {
        has_contextual(&self.0, "raises")
    }
    /// The body block, `do={...}`.
    pub fn body(&self) -> Option<Block> {
        named_arg(&self.0, "do").and_then(|a| a.block())
    }
}

ast_node!(
    /// The parameters of a function.
    ParamList
);
ast_node!(
    /// The `-> Type` of a function.
    RetType
);

/// How an argument is passed to a parameter.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Convention {
    /// Read-only borrow (the default).
    Read,
    /// `mut`: mutable borrow.
    Mut,
    /// `owned`: transfer of ownership.
    Owned,
}

ast_node!(
    /// A function parameter: `mut name:Type=default` or `self`.
    Param
);

impl Param {
    /// The parameter's convention.
    pub fn convention(&self) -> Convention {
        if has_contextual(&self.0, "mut") {
            Convention::Mut
        } else if has_contextual(&self.0, "owned") {
            Convention::Owned
        } else {
            Convention::Read
        }
    }
    /// Returns true for a `self` parameter.
    pub fn is_self(&self) -> bool {
        token(&self.0, SyntaxKind::KwSelf).is_some()
    }
    /// The parameter's name.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    /// The parameter's type.
    pub fn ty(&self) -> Option<Type> {
        child::<TypeAnnotation>(&self.0).and_then(|a| a.ty())
    }
    /// The default value, if any.
    pub fn default_value(&self) -> Option<Expr> {
        child::<DefaultValue>(&self.0).and_then(|d| d.0.children().find_map(Expr::cast))
    }
}

ast_node!(
    /// `:name` followed by a type.
    TypeAnnotation
);

impl TypeAnnotation {
    /// The annotated type.
    pub fn ty(&self) -> Option<Type> {
        self.0.children().find_map(Type::cast)
    }
}

ast_node!(
    /// `=value` after a parameter or field type.
    DefaultValue
);
ast_node!(
    /// `:struct ...`
    StructDecl
);

impl StructDecl {
    /// The struct's name.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    /// Generic parameters, if any.
    pub fn generic_params(&self) -> Option<GenericParamList> {
        child(&self.0)
    }
    /// The types listed after `impl=`.
    pub fn impl_list(&self) -> Vec<Type> {
        named_arg(&self.0, "impl")
            .and_then(|arg| child::<ImplList>(&arg.0))
            .map(|list| list.0.children().filter_map(Type::cast).collect())
            .unwrap_or_default()
    }
    /// The fields, in order.
    pub fn fields(&self) -> Vec<FieldDecl> {
        child::<MemberList>(&self.0)
            .map(|list| children(&list.0).collect())
            .unwrap_or_default()
    }
    /// The methods and associated functions.
    pub fn functions(&self) -> Vec<FnDecl> {
        child::<MemberList>(&self.0)
            .map(|list| children(&list.0).collect())
            .unwrap_or_default()
    }
}

ast_node!(
    /// The `{ ... }` body of a type declaration.
    MemberList
);
ast_node!(
    /// `Trait1,Trait2` after `impl=`.
    ImplList
);
ast_node!(
    /// A field of a struct: `name:Type` or `name:Type=default`.
    FieldDecl
);

impl FieldDecl {
    /// The field's name.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    /// The field's type.
    pub fn ty(&self) -> Option<Type> {
        child::<TypeAnnotation>(&self.0).and_then(|a| a.ty())
    }
    /// The default value, if any.
    pub fn default_value(&self) -> Option<Expr> {
        child::<DefaultValue>(&self.0).and_then(|d| d.0.children().find_map(Expr::cast))
    }
}
ast_node!(
    /// `:enum ...`
    EnumDecl
);

impl EnumDecl {
    /// The enum's name.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    /// Generic parameters, if any.
    pub fn generic_params(&self) -> Option<GenericParamList> {
        child(&self.0)
    }
    /// The types listed after `impl=`.
    pub fn impl_list(&self) -> Vec<Type> {
        named_arg(&self.0, "impl")
            .and_then(|arg| child::<ImplList>(&arg.0))
            .map(|list| list.0.children().filter_map(Type::cast).collect())
            .unwrap_or_default()
    }
    /// The variants, in order.
    pub fn variants(&self) -> Vec<VariantDecl> {
        child::<MemberList>(&self.0)
            .map(|list| children(&list.0).collect())
            .unwrap_or_default()
    }
    /// The methods and associated functions.
    pub fn functions(&self) -> Vec<FnDecl> {
        child::<MemberList>(&self.0)
            .map(|list| children(&list.0).collect())
            .unwrap_or_default()
    }
}

ast_node!(
    /// A variant of an enum: `name field:Type ...`.
    VariantDecl
);

impl VariantDecl {
    /// The variant's name.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    /// The fields, in order.
    pub fn fields(&self) -> Vec<FieldDecl> {
        children(&self.0).collect()
    }
}
ast_node!(
    /// `:trait ...`
    TraitDecl
);

impl TraitDecl {
    /// The trait's name.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }
    /// Generic parameters, if any.
    pub fn generic_params(&self) -> Option<GenericParamList> {
        child(&self.0)
    }
    /// The traits listed after `impl=`: the traits this one requires.
    pub fn impl_list(&self) -> Vec<Type> {
        named_arg(&self.0, "impl")
            .and_then(|arg| child::<ImplList>(&arg.0))
            .map(|list| list.0.children().filter_map(Type::cast).collect())
            .unwrap_or_default()
    }
    /// The method signatures, default methods and associated functions.
    pub fn functions(&self) -> Vec<FnDecl> {
        child::<MemberList>(&self.0)
            .map(|list| children(&list.0).collect())
            .unwrap_or_default()
    }
}
ast_node!(
    /// `:impl Type {...}`
    ImplDecl
);

impl ImplDecl {
    /// Generic parameters, if any.
    pub fn generic_params(&self) -> Option<GenericParamList> {
        child(&self.0)
    }
    /// The type that the functions are for.
    pub fn self_type(&self) -> Option<Type> {
        self.0.children().find_map(Type::cast)
    }
    /// The traits listed after `impl=`.
    pub fn impl_list(&self) -> Vec<Type> {
        named_arg(&self.0, "impl")
            .and_then(|arg| child::<ImplList>(&arg.0))
            .map(|list| list.0.children().filter_map(Type::cast).collect())
            .unwrap_or_default()
    }
    /// The methods and associated functions.
    pub fn functions(&self) -> Vec<FnDecl> {
        child::<MemberList>(&self.0)
            .map(|list| children(&list.0).collect())
            .unwrap_or_default()
    }
}

ast_node!(
    /// `:use /path`
    UseDecl
);

impl UseDecl {
    /// The path of the module or item.
    pub fn path(&self) -> Option<Path> {
        child(&self.0)
    }
    /// The name given with `as=`.
    pub fn alias(&self) -> Option<Name> {
        named_arg(&self.0, "as").and_then(|arg| child(&arg.0))
    }
}
ast_node!(
    /// `:extern lib="..." {...}`
    ExternDecl
);

impl ExternDecl {
    /// The value of `lib=`, the library that defines the functions.
    pub fn lib(&self) -> Option<Expr> {
        named_arg(&self.0, "lib").and_then(|arg| arg.expr())
    }
    /// The declared functions.
    pub fn functions(&self) -> Vec<FnDecl> {
        child::<MemberList>(&self.0)
            .map(|list| children(&list.0).collect())
            .unwrap_or_default()
    }
}
ast_node!(
    /// `:test "name" do={...}`
    TestDecl
);

impl TestDecl {
    /// The test's name.
    pub fn name(&self) -> Option<StringLit> {
        child(&self.0)
    }
    /// The test's statements.
    pub fn body(&self) -> Option<Block> {
        named_arg(&self.0, "do").and_then(|arg| arg.block())
    }
}
ast_node!(
    /// `<T, U: Bound>`
    GenericParamList
);
impl GenericParamList {
    /// The names of the generic parameters.
    pub fn names(&self) -> Vec<String> {
        self.params()
            .iter()
            .filter_map(GenericParam::name)
            .map(|name| name.text())
            .collect()
    }

    /// The generic parameters, in order.
    pub fn params(&self) -> Vec<GenericParam> {
        children(&self.0).collect()
    }
}

ast_node!(
    /// `T` or `T: Bound + Other` in a generic parameter list.
    GenericParam
);

impl GenericParam {
    /// The parameter's name.
    pub fn name(&self) -> Option<Name> {
        child(&self.0)
    }

    /// The bounds after `:`.
    pub fn bounds(&self) -> Vec<Type> {
        self.0.children().filter_map(Type::cast).collect()
    }
}

ast_node!(
    /// `<A, B>`
    GenericArgList
);

impl GenericArgList {
    /// The type arguments, in order.
    pub fn types(&self) -> Vec<Type> {
        self.0.children().filter_map(Type::cast).collect()
    }
}

form_keyword!(
    LocalDecl,
    ConstDecl,
    GlobalDecl,
    SetStmt,
    IfStmt,
    WhileStmt,
    DoWhileStmt,
    ForStmt,
    ForeachStmt,
    MatchStmt,
    OnErrorStmt,
    UnsafeBlock,
    FnDecl,
    StructDecl,
    EnumDecl,
    TraitDecl,
    ImplDecl,
    UseDecl,
    ExternDecl,
    TestDecl
);

// ----- Names -------------------------------------------------------------------------------

ast_node!(
    /// A declared name.
    Name
);
ast_node!(
    /// A reference to a name.
    NameRef
);

impl Name {
    /// The name's text. A name written as a variable (`$x`, already reported) loses its `$`.
    pub fn text(&self) -> String {
        name_text(&self.0)
    }
}

impl NameRef {
    /// The name's text.
    pub fn text(&self) -> String {
        name_text(&self.0)
    }
}

fn name_text(node: &SyntaxNode) -> String {
    node.children_with_tokens()
        .filter_map(rowan::NodeOrToken::into_token)
        .find(|t| !t.kind().is_trivia())
        .map(|t| t.text().trim_start_matches('$').to_owned())
        .unwrap_or_default()
}

// ----- Types -------------------------------------------------------------------------------

/// A type.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Type {
    /// `Name`, `Name<Args>` or `/module/Name`.
    Path(PathType),
    /// `fn(A) -> B`
    Fn(FnType),
    /// `T?`
    Option(OptionType),
}

impl Type {
    fn cast(node: SyntaxNode) -> Option<Self> {
        match node.kind() {
            SyntaxKind::PathType => Some(Self::Path(PathType(node))),
            SyntaxKind::FnType => Some(Self::Fn(FnType(node))),
            SyntaxKind::OptionType => Some(Self::Option(OptionType(node))),
            _ => None,
        }
    }

    /// The source range of the type.
    pub fn span(&self) -> Span {
        match self {
            Self::Path(t) => t.span(),
            Self::Fn(t) => t.span(),
            Self::Option(t) => t.span(),
        }
    }
}

ast_node!(
    /// `Name`, `Name<Args>` or `/module/Name`.
    PathType
);

impl PathType {
    /// The type's name, when it is a plain name rather than a module path.
    pub fn name(&self) -> Option<NameRef> {
        child(&self.0)
    }
    /// The module path, as in `/std/fmt/Display`.
    pub fn path(&self) -> Option<Path> {
        child(&self.0)
    }
    /// Generic arguments, if any.
    pub fn generic_args(&self) -> Option<GenericArgList> {
        child(&self.0)
    }
}

ast_node!(
    /// `fn(A, B) -> R`
    FnType
);

impl FnType {
    /// The types of the parameters.
    pub fn params(&self) -> Vec<Type> {
        self.0.children().filter_map(Type::cast).collect()
    }
    /// The type after `->`, if written.
    pub fn ret(&self) -> Option<Type> {
        child::<RetType>(&self.0).and_then(|r| r.0.children().find_map(Type::cast))
    }
    /// Returns true for `fn(...) raises`.
    pub fn is_raises(&self) -> bool {
        has_contextual(&self.0, "raises")
    }
}
ast_node!(
    /// `T?`
    OptionType
);

impl OptionType {
    /// The type of the value.
    pub fn inner(&self) -> Option<Type> {
        self.0.children().find_map(Type::cast)
    }
}
ast_node!(
    /// `/name/name`
    Path
);

impl Path {
    /// The names, in order, each with its span.
    pub fn segments(&self) -> Vec<(String, Span)> {
        self.0
            .children_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .filter(|t| t.kind() == SyntaxKind::Ident)
            .map(|t| (t.text().to_owned(), span_of(t.text_range())))
            .collect()
    }
}

// ----- Expressions -------------------------------------------------------------------------

/// An expression or atom.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs, reason = "each variant wraps the node of the same name")]
pub enum Expr {
    Literal(Literal),
    StringLit(StringLit),
    VarExpr(VarExpr),
    PathExpr(PathExpr),
    ParenExpr(ParenExpr),
    BracketExpr(BracketExpr),
    BraceLit(BraceLit),
    MemberExpr(MemberExpr),
    PrefixExpr(PrefixExpr),
    BinExpr(BinExpr),
    CastExpr(CastExpr),
    /// A type name in value position, already reported by the parser unless it is the base
    /// of a `->` access.
    TypeName(PathType),
}

impl Expr {
    /// Wraps `node` if it is an expression.
    pub fn cast(node: SyntaxNode) -> Option<Self> {
        Some(match node.kind() {
            SyntaxKind::Literal => Self::Literal(Literal(node)),
            SyntaxKind::StringLit => Self::StringLit(StringLit(node)),
            SyntaxKind::VarExpr => Self::VarExpr(VarExpr(node)),
            SyntaxKind::PathExpr => Self::PathExpr(PathExpr(node)),
            SyntaxKind::ParenExpr => Self::ParenExpr(ParenExpr(node)),
            SyntaxKind::BracketExpr => Self::BracketExpr(BracketExpr(node)),
            SyntaxKind::BraceLit => Self::BraceLit(BraceLit(node)),
            SyntaxKind::MemberExpr => Self::MemberExpr(MemberExpr(node)),
            SyntaxKind::PrefixExpr => Self::PrefixExpr(PrefixExpr(node)),
            SyntaxKind::BinExpr => Self::BinExpr(BinExpr(node)),
            SyntaxKind::CastExpr => Self::CastExpr(CastExpr(node)),
            SyntaxKind::PathType => Self::TypeName(PathType(node)),
            _ => return None,
        })
    }

    /// The wrapped node.
    pub fn syntax(&self) -> &SyntaxNode {
        match self {
            Self::Literal(n) => n.syntax(),
            Self::StringLit(n) => n.syntax(),
            Self::VarExpr(n) => n.syntax(),
            Self::PathExpr(n) => n.syntax(),
            Self::ParenExpr(n) => n.syntax(),
            Self::BracketExpr(n) => n.syntax(),
            Self::BraceLit(n) => n.syntax(),
            Self::MemberExpr(n) => n.syntax(),
            Self::PrefixExpr(n) => n.syntax(),
            Self::BinExpr(n) => n.syntax(),
            Self::CastExpr(n) => n.syntax(),
            Self::TypeName(n) => n.syntax(),
        }
    }

    /// The source range of the expression.
    pub fn span(&self) -> Span {
        span_of(trimmed_range(self.syntax()))
    }
}

ast_node!(
    /// A literal: number, duration, character, raw string, `true`, `false` or `none`.
    Literal
);

/// The kind of a literal, with its token text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LiteralKind {
    /// An integer, with its token text (without the sign).
    Int(String),
    /// A float, with its token text (without the sign).
    Float(String),
    /// A duration, with its token text (without the sign).
    Duration(String),
    /// A character literal, including quotes.
    Char(String),
    /// A raw string, including `r`, hashes and quotes.
    RawString(String),
    /// `true` or `false`.
    Bool(bool),
    /// `none`
    None,
}

impl Literal {
    /// Returns true for a negative number such as `-5`.
    pub fn is_negative(&self) -> bool {
        token(&self.0, SyntaxKind::Minus).is_some()
    }

    /// The kind and text of the literal.
    pub fn kind(&self) -> Option<LiteralKind> {
        literal_kind(&self.0)
    }
}

/// The kind and text of the literal token of `node`, ignoring a leading minus.
fn literal_kind(node: &SyntaxNode) -> Option<LiteralKind> {
    let token = node
        .children_with_tokens()
        .filter_map(rowan::NodeOrToken::into_token)
        .find(|t| !t.kind().is_trivia() && t.kind() != SyntaxKind::Minus)?;
    let text = token.text().to_owned();
    Some(match token.kind() {
        SyntaxKind::Int => LiteralKind::Int(text),
        SyntaxKind::Float => LiteralKind::Float(text),
        SyntaxKind::Duration => LiteralKind::Duration(text),
        SyntaxKind::Char => LiteralKind::Char(text),
        SyntaxKind::RawString => LiteralKind::RawString(text),
        SyntaxKind::KwTrue => LiteralKind::Bool(true),
        SyntaxKind::KwFalse => LiteralKind::Bool(false),
        SyntaxKind::KwNone => LiteralKind::None,
        _ => return None,
    })
}

ast_node!(
    /// `"text $var $(expr) $[command]"`
    StringLit
);

/// A piece of a string literal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum StringPart {
    /// Literal text, with escapes still encoded.
    Text(SyntaxToken),
    /// `$name`
    Var(SyntaxToken),
    /// `$(expr)` or `$[command]`
    Interpolation(Interpolation),
}

impl StringLit {
    /// The pieces of the string, in order.
    pub fn parts(&self) -> Vec<StringPart> {
        self.0
            .children_with_tokens()
            .filter_map(|element| match element {
                rowan::NodeOrToken::Token(t) => match t.kind() {
                    SyntaxKind::StringText => Some(StringPart::Text(t)),
                    SyntaxKind::InterpVar => Some(StringPart::Var(t)),
                    _ => None,
                },
                rowan::NodeOrToken::Node(n) => {
                    Interpolation::cast(n).map(StringPart::Interpolation)
                }
            })
            .collect()
    }
}

ast_node!(
    /// `$(expr)` or `$[command]` inside a string.
    Interpolation
);

impl Interpolation {
    /// The interpolated expression, for `$(...)`.
    pub fn expr(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
    /// The interpolated command, for `$[...]`.
    pub fn command(&self) -> Option<Stmt> {
        self.0.children().find_map(Stmt::cast)
    }
}

ast_node!(
    /// `$name`
    VarExpr
);

impl VarExpr {
    /// The variable's name without `$`.
    pub fn name(&self) -> String {
        token(&self.0, SyntaxKind::Variable)
            .map(|t| t.text()[1..].to_owned())
            .unwrap_or_default()
    }
}

ast_node!(
    /// `/module/item`, with optional type arguments: an item of a module, or a type of a
    /// module followed by `->`.
    PathExpr
);

impl PathExpr {
    /// The path, with its type arguments.
    pub fn path_type(&self) -> Option<PathType> {
        child(&self.0)
    }
}
ast_node!(
    /// `( expr )`
    ParenExpr
);

impl ParenExpr {
    /// The inner expression.
    pub fn expr(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
}

ast_node!(
    /// `[ command ]`
    BracketExpr
);

impl BracketExpr {
    /// The command inside the brackets.
    pub fn command(&self) -> Option<Stmt> {
        self.0.children().find_map(Stmt::cast)
    }
}

ast_node!(
    /// A collection or struct literal.
    BraceLit
);

/// An element of a brace literal.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum BraceElement {
    /// `name=value`, in a struct literal.
    Field(FieldInit),
    /// `key=value`, in a map literal.
    Entry(MapEntry),
    /// A value, in a list literal.
    Value(Expr),
}

impl BraceLit {
    /// The type before the brace, as in `Point{...}` or `List<i64>{}`.
    pub fn type_prefix(&self) -> Option<PathType> {
        child(&self.0)
    }

    /// The elements, in order.
    pub fn elements(&self) -> Vec<BraceElement> {
        self.0
            .children()
            .filter_map(|node| match node.kind() {
                SyntaxKind::FieldInit => Some(BraceElement::Field(FieldInit(node))),
                SyntaxKind::MapEntry => Some(BraceElement::Entry(MapEntry(node))),
                SyntaxKind::PathType => None,
                _ => Expr::cast(node).map(BraceElement::Value),
            })
            .collect()
    }

    /// The span of the opening brace.
    pub fn open_brace_span(&self) -> Option<Span> {
        token(&self.0, SyntaxKind::LBrace).map(|t| span_of(t.text_range()))
    }
}

ast_node!(
    /// `name=value` in a struct literal.
    FieldInit
);

impl FieldInit {
    /// The field's name.
    pub fn name(&self) -> Option<NameRef> {
        child(&self.0)
    }
    /// The value.
    pub fn value(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
}

ast_node!(
    /// `key=value` in a map literal.
    MapEntry
);

impl MapEntry {
    /// The key.
    pub fn key(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
    /// The value.
    pub fn value(&self) -> Option<Expr> {
        self.0.children().filter_map(Expr::cast).nth(1)
    }
}
ast_node!(
    /// `base->member`
    MemberExpr
);
/// The part after `->` in a member access.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum Member {
    /// A field or method name: `$p->x`.
    Name(NameRef),
    /// An index or key: `$list->0`, `$map->"key"`, `$list->$i`, `$list->(expr)`.
    Index(Expr),
}

impl MemberExpr {
    /// The value or type whose member is accessed.
    pub fn base(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }

    /// The member accessed.
    pub fn member(&self) -> Option<Member> {
        let node = self.0.children().nth(1)?;
        if let Some(name) = NameRef::cast(node.clone()) {
            return Some(Member::Name(name));
        }
        Expr::cast(node).map(Member::Index)
    }

    /// Explicit type arguments of a method, as in `$a->map<f64>`.
    pub fn generic_args(&self) -> Option<GenericArgList> {
        child(&self.0)
    }
}

ast_node!(
    /// `-x`, `!x`, `~x`
    PrefixExpr
);

/// A prefix operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum PrefixOp {
    /// `-`
    Neg,
    /// `!`
    Not,
    /// `~`
    BitNot,
}

impl PrefixExpr {
    /// The operator.
    pub fn op(&self) -> Option<PrefixOp> {
        self.0
            .children_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .find_map(|t| match t.kind() {
                SyntaxKind::Minus => Some(PrefixOp::Neg),
                SyntaxKind::Bang => Some(PrefixOp::Not),
                SyntaxKind::Tilde => Some(PrefixOp::BitNot),
                _ => None,
            })
    }
    /// The operand.
    pub fn operand(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
}

ast_node!(
    /// `lhs op rhs`
    BinExpr
);

/// A binary operator.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(
    missing_docs,
    reason = "the operators are listed in section 7 of the specification"
)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    Shl,
    Shr,
    BitAnd,
    BitOr,
    BitXor,
    Concat,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    In,
    And,
    Or,
}

impl BinaryOp {
    /// The operator as written (the canonical spelling for `and` and `or`).
    pub fn symbol(self) -> &'static str {
        match self {
            Self::Add => "+",
            Self::Sub => "-",
            Self::Mul => "*",
            Self::Div => "/",
            Self::Rem => "%",
            Self::Shl => "<<",
            Self::Shr => ">>",
            Self::BitAnd => "&",
            Self::BitOr => "|",
            Self::BitXor => "^",
            Self::Concat => ".",
            Self::Eq => "=",
            Self::Ne => "!=",
            Self::Lt => "<",
            Self::Le => "<=",
            Self::Gt => ">",
            Self::Ge => ">=",
            Self::In => "in",
            Self::And => "and",
            Self::Or => "or",
        }
    }
}

impl BinExpr {
    /// The left operand.
    pub fn lhs(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
    /// The right operand.
    pub fn rhs(&self) -> Option<Expr> {
        self.0.children().filter_map(Expr::cast).nth(1)
    }
    /// The operator, from the tokens between the operands. `>` `>` is `>>`; `>` `=` is `>=`.
    pub fn op(&self) -> Option<BinaryOp> {
        let mut tokens = self
            .0
            .children_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .filter(|t| !t.kind().is_trivia() && t.kind() != SyntaxKind::Newline);
        let first = tokens.next()?;
        Some(match first.kind() {
            SyntaxKind::Plus => BinaryOp::Add,
            SyntaxKind::Minus => BinaryOp::Sub,
            SyntaxKind::Star => BinaryOp::Mul,
            SyntaxKind::Slash => BinaryOp::Div,
            SyntaxKind::Percent => BinaryOp::Rem,
            SyntaxKind::Shl => BinaryOp::Shl,
            SyntaxKind::Amp => BinaryOp::BitAnd,
            SyntaxKind::Pipe => BinaryOp::BitOr,
            SyntaxKind::Caret => BinaryOp::BitXor,
            SyntaxKind::Dot => BinaryOp::Concat,
            SyntaxKind::Eq => BinaryOp::Eq,
            SyntaxKind::Ne => BinaryOp::Ne,
            SyntaxKind::Lt => BinaryOp::Lt,
            SyntaxKind::Le => BinaryOp::Le,
            SyntaxKind::Gt => match tokens.next().map(|t| t.kind()) {
                Some(SyntaxKind::Gt) => BinaryOp::Shr,
                Some(SyntaxKind::Eq) => BinaryOp::Ge,
                _ => BinaryOp::Gt,
            },
            SyntaxKind::KwIn => BinaryOp::In,
            SyntaxKind::KwAnd | SyntaxKind::AndAnd => BinaryOp::And,
            SyntaxKind::KwOr | SyntaxKind::OrOr => BinaryOp::Or,
            _ => return None,
        })
    }
    /// The span of the operator token(s).
    pub fn op_span(&self) -> Option<Span> {
        let lhs_end = self.lhs()?.syntax().text_range().end();
        let rhs_start = self.rhs().map(|r| r.syntax().text_range().start());
        let tokens: Vec<_> = self
            .0
            .children_with_tokens()
            .filter_map(rowan::NodeOrToken::into_token)
            .filter(|t| {
                !t.kind().is_trivia()
                    && t.kind() != SyntaxKind::Newline
                    && t.text_range().start() >= lhs_end
                    && rhs_start.is_none_or(|start| t.text_range().end() <= start)
            })
            .collect();
        let first = tokens.first()?;
        let last = tokens.last()?;
        Some(span_of(TextRange::new(
            first.text_range().start(),
            last.text_range().end(),
        )))
    }
}

ast_node!(
    /// `expr as Type`
    CastExpr
);

impl CastExpr {
    /// The converted expression.
    pub fn expr(&self) -> Option<Expr> {
        self.0.children().find_map(Expr::cast)
    }
    /// The target type.
    pub fn ty(&self) -> Option<Type> {
        self.0.children().skip(1).find_map(Type::cast)
    }
}
