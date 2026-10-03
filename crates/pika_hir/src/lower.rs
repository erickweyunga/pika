//! Lowering from the AST to the HIR, with name resolution.

mod modules;
mod naming;

use std::collections::{HashMap, HashSet};
use std::rc::Rc;

use pika_diagnostics::{Diagnostic, Span};
use pika_syntax::ast::{self, AstNode, LiteralKind};
use pika_syntax::literal;

use crate::codes;
use pika_ty::{Adt, AdtKind, IntTy, Param as TyParam};

use crate::{
    Block, Body, Bound, Builtin, BuiltinTrait, CallArg, CallHead, Callee, Capture, ConstId,
    ConstItem, Convention, Derives, Element, ElseBranch, Expr, ExprId, FieldDef, FnId, FnKind,
    FnOwner, ForEnd, Function, GenericParam, Generics, GlobalId, GlobalItem, ImplDef, ImplId,
    Literal, Local, LocalId, LocalKind, MatchArm, Module, ModuleDef, ModuleId, Param, Pat, PatId,
    Place, PreludeTrait, RESERVED_COMMANDS, Spanned, Stmt, StmtId, StringPart, TestDef, TraitDef,
    TraitId, Ty, TypeArgs, TypeDef, TypeHead, TypeId, VariantDef,
};
use modules::{Items, ModuleNames, PathValue, Shared, Source};
pub use modules::{Root, SourceModule, SourcePackage};

/// The name of the package of a program lowered from a single file by [`lower`].
pub const SINGLE_FILE_PACKAGE: &str = "main";

/// Lowers a program written in a single source file to a [`Module`], resolving every name.
/// The file is the root module of a package named [`SINGLE_FILE_PACKAGE`].
///
/// The file should be free of syntax errors: lowering tolerates missing parts, but the
/// resulting diagnostics are only meaningful for a well-formed tree.
pub fn lower(file: &ast::SourceFile) -> (Module, Vec<Diagnostic>) {
    let package = SourcePackage {
        name: SINGLE_FILE_PACKAGE.to_owned(),
        dependencies: Vec::new(),
        modules: vec![SourceModule {
            path: Vec::new(),
            file: file.clone(),
            base: 0,
        }],
    };
    lower_program(
        &[package],
        Root {
            package: 0,
            binary: true,
        },
    )
}

/// Lowers the modules of `packages` to a [`Module`], resolving every name. `root` is the
/// package being compiled; the others are its dependencies. If it is a binary, its root
/// module is the program's entry module, which declares `main` or is a script (spec section
/// 4.4).
///
/// The files should be free of syntax errors: lowering tolerates missing parts, but the
/// resulting diagnostics are only meaningful for well-formed trees.
pub fn lower_program(packages: &[SourcePackage], root: Root) -> (Module, Vec<Diagnostic>) {
    let mut diagnostics = Vec::new();
    let mut module = Module::default();
    let mut shared = Shared::default();
    let prelude = add_prelude(&mut module, &mut shared);
    let sources = modules::add_modules(packages, root, &mut module, &mut shared);

    // Pass 1: the items each module declares, and the members of its types and traits.
    let collected: Vec<Collected> = sources
        .iter()
        .map(|source| {
            ast::with_span_base(source.base, || {
                collect(source, &mut module, &mut shared, &mut diagnostics)
            })
        })
        .collect();
    for item in &collected {
        shared.declared.insert(item.module, item.items.clone());
    }
    // The names in scope in each module, with its imports, and the traits each type and
    // trait lists in `impl=`.
    let mut names: Vec<ModuleNames> = sources
        .iter()
        .zip(&collected)
        .map(|(source, collected)| {
            ast::with_span_base(source.base, || {
                module_names(
                    source,
                    collected,
                    &prelude,
                    &shared,
                    &module,
                    &mut diagnostics,
                )
            })
        })
        .collect();
    link(&sources, &collected, &names, &mut shared);
    let shared = Rc::new(shared);
    for names in &mut names {
        names.shared = Rc::clone(&shared);
    }

    // Pass 2: each declaration, in an order where what a declaration refers to is lowered
    // before it.
    let modules: Vec<(&Source<'_>, &Collected, &ModuleNames)> = sources
        .iter()
        .zip(&collected)
        .zip(&names)
        .map(|((source, collected), names)| (source, collected, names))
        .collect();
    let passes: [LowerPass; 7] = [
        lower_types,
        lower_traits,
        lower_impls,
        lower_value_types,
        lower_fns,
        lower_initializers,
        lower_tests,
    ];
    for pass in passes {
        for &(source, collected, names) in &modules {
            ast::with_span_base(source.base, || {
                pass(collected, names, &mut module, &mut diagnostics);
            });
        }
    }
    for &(source, collected, names) in modules.iter().filter(|(source, ..)| source.entry) {
        ast::with_span_base(source.base, || {
            lower_entry(collected, names, &mut module, &mut diagnostics);
        });
    }

    diagnostics.extend(naming::check(&module));
    diagnostics.sort_by_key(|d| d.primary.span.start);
    (module, diagnostics)
}

/// A pass over the declarations of one module.
type LowerPass = fn(&Collected, &ModuleNames, &mut Module, &mut Vec<Diagnostic>);

/// The names in scope in the module `source`: the prelude's, its own, and the ones it
/// imports.
fn module_names(
    source: &Source<'_>,
    collected: &Collected,
    prelude: &Items,
    shared: &Shared,
    module: &Module,
    diagnostics: &mut Vec<Diagnostic>,
) -> ModuleNames {
    let mut items = prelude.clone();
    items.fns.extend(collected.items.fns.clone());
    items.types.extend(collected.items.types.clone());
    items.traits.extend(collected.items.traits.clone());
    items.values.extend(collected.items.values.clone());
    let mut names = ModuleNames {
        module: source.id,
        items,
        aliases: HashMap::new(),
        packages: source.packages.clone(),
        script_locals: collected.script_locals.clone(),
        shared: Rc::default(),
    };
    for decl in &collected.uses {
        modules::add_use(&mut names, decl, shared, module, diagnostics);
    }
    names
}

/// Resolves the user-defined traits each type and trait lists in `impl=`, in the module that
/// declares it, for the lookups of functions that types get from traits.
fn link(
    sources: &[Source<'_>],
    collected: &[Collected],
    names: &[ModuleNames],
    shared: &mut Shared,
) {
    let mut type_impls = Vec::new();
    let mut supertraits = Vec::new();
    for ((source, collected), names) in sources.iter().zip(collected).zip(names) {
        ast::with_span_base(source.base, || {
            let listed = |list: &[ast::Type]| -> Vec<TraitId> {
                list.iter()
                    .filter_map(|item| modules::listed_trait(names, shared, item))
                    .collect()
            };
            for (id, decl) in &collected.types {
                type_impls.push((*id, listed(&decl.impl_list())));
            }
            for (id, decl) in &collected.traits {
                supertraits.push((*id, listed(&decl.impl_list())));
            }
        });
    }
    shared.type_impls.extend(type_impls);
    for (id, traits) in supertraits {
        if let Some(members) = shared.trait_members.get_mut(&id) {
            members.supertraits = traits;
        }
    }
}

fn lower_types(
    collected: &Collected,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (id, decl) in &collected.types {
        match decl {
            TypeDecl::Struct(decl) => lower_struct(decl, *id, names, module, diagnostics),
            TypeDecl::Enum(decl) => lower_enum(decl, *id, names, module, diagnostics),
        }
    }
}

fn lower_traits(
    collected: &Collected,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (id, decl) in &collected.traits {
        lower_trait(decl, *id, names, module, diagnostics);
    }
}

fn lower_impls(
    collected: &Collected,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (id, decl) in &collected.impls {
        lower_impl(decl, *id, names, module, diagnostics);
    }
}

/// Lowers the type parameters and the type of an `:impl`, which must be a built-in type with
/// each parameter in it.
fn lower_impl(
    decl: &ast::ImplDecl,
    id: ImplId,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let generics = {
        let scope = TypeScope {
            names,
            self_ty: None,
            params: Vec::new(),
        };
        lower_generics(decl.generic_params().as_ref(), &[], &scope, diagnostics)
    };
    let scope = TypeScope {
        names,
        self_ty: None,
        params: generics
            .iter()
            .map(|p| (p.name.value.clone(), p.ty))
            .collect(),
    };
    let self_ty = decl.self_type().map_or(
        Spanned {
            value: Ty::Error,
            span: decl.keyword_span(),
        },
        |ty| lower_type(&ty, &scope, diagnostics),
    );
    if TypeHead::of(self_ty.value).is_none() && self_ty.value != Ty::Error {
        let mut diagnostic = Diagnostic::error(
            codes::INVALID_IMPL,
            format!(
                "`:impl` declares functions of built-in types, not of `{}`",
                self_ty.value
            ),
            self_ty.span,
        );
        if let Ty::Adt(adt) = self_ty.value {
            diagnostic = diagnostic.with_help(format!(
                "declare the functions of `{}` in its body",
                adt.name
            ));
        }
        diagnostics.push(diagnostic);
    }
    for param in &generics {
        if !ty_contains(self_ty.value, param.ty) {
            diagnostics.push(Diagnostic::error(
                codes::INVALID_IMPL,
                format!(
                    "the type parameter `{}` is not used in `{}`",
                    param.name.value, self_ty.value
                ),
                param.name.span,
            ));
        }
    }
    let def = &mut module.impls[id];
    def.generics = Generics { params: generics };
    def.self_ty = self_ty;
}

/// Returns true if `part` is `ty` or occurs in it.
fn ty_contains(ty: Ty, part: Ty) -> bool {
    ty == part || ty.components().into_iter().any(|c| ty_contains(c, part))
}

/// The declared types of constants and globals.
fn lower_value_types(
    collected: &Collected,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let scope = TypeScope {
        names,
        self_ty: None,
        params: Vec::new(),
    };
    for (id, decl) in &collected.consts {
        module.consts[*id].ty = decl.ty().map(|t| lower_type(&t, &scope, diagnostics));
    }
    for (id, decl) in &collected.globals {
        module.globals[*id].ty = decl.ty().map(|t| lower_type(&t, &scope, diagnostics));
    }
}

fn lower_fns(
    collected: &Collected,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (id, decl, owner) in &collected.fns {
        let owner = owner.map(|owner| Owner::new(owner, module));
        let first_closure = next_fn_id(module);
        let kind = module.functions[*id].kind;
        let (function, closures) = lower_fn(
            decl,
            names,
            owner.as_ref(),
            (*id, kind, first_closure),
            diagnostics,
        );
        module.functions[*id] = function;
        add_closures(module, first_closure, closures);
    }
}

fn lower_initializers(
    collected: &Collected,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    for (id, decl) in &collected.consts {
        let (body, init) = lower_initializer(decl.value(), names, diagnostics);
        let item = &mut module.consts[*id];
        item.body = body;
        item.init = init;
        if init.is_none() {
            diagnostics.push(
                Diagnostic::error(
                    codes::INCOMPLETE_DECLARATION,
                    format!("constant `{}` has no value", item.name.value),
                    item.name.span,
                )
                .with_help(format!("give it a value: `:const {} 100`", item.name.value)),
            );
        }
    }
    for (id, decl) in &collected.globals {
        let (body, init) = lower_initializer(decl.value(), names, diagnostics);
        let item = &mut module.globals[*id];
        item.body = body;
        item.init = init;
        if item.ty.is_none() || init.is_none() {
            diagnostics.push(
                Diagnostic::error(
                    codes::INCOMPLETE_DECLARATION,
                    format!(
                        "global `{}` needs a type and an initial value",
                        item.name.value
                    ),
                    item.name.span,
                )
                .with_help(format!("declare it as `:global {}:i64 0`", item.name.value)),
            );
        }
    }
}

/// The entry point of the entry module: its `:fn main`, or the implicit `main` of its
/// top-level statements.
fn lower_entry(
    collected: &Collected,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let explicit_main = collected.items.fns.get("main").copied();
    if let Some(first) = collected.script.first() {
        if let Some(main) = explicit_main {
            diagnostics.push(
                Diagnostic::error(
                    codes::MAIN_AND_SCRIPT,
                    "a file with `:fn main` cannot also have top-level statements",
                    first.span(),
                )
                .with_secondary(module.functions[main].name.span, "`main` is declared here")
                .with_help("move the statements into `main`, or remove `:fn main`"),
            );
        } else {
            let id = FnId::from_raw(la_arena::RawIdx::from_u32(next_fn_id(module)));
            let name = Spanned {
                value: "main".to_owned(),
                span: first.span(),
            };
            let implicit = Implicit {
                id,
                kind: FnKind::ImplicitMain,
                name,
                raises: false,
            };
            let (function, closures) =
                lower_statements(&collected.script, names, implicit, diagnostics);
            let allocated = module.functions.alloc(function);
            debug_assert_eq!(allocated, id);
            module.entry = Some(id);
            add_closures(module, id.into_raw().into_u32() + 1, closures);
        }
    }
    if module.entry.is_none() {
        module.entry = explicit_main;
    }
}

/// The id the next function added to the module gets.
fn next_fn_id(module: &Module) -> u32 {
    u32::try_from(module.functions.len()).expect("fewer than 2^32 functions")
}

/// Adds the closures of a function to the module, whose ids start at `first`.
fn add_closures(module: &mut Module, first: u32, closures: Vec<Function>) {
    for (offset, closure) in closures.into_iter().enumerate() {
        let id = module.functions.alloc(closure);
        debug_assert_eq!(
            id.into_raw().into_u32(),
            first + u32::try_from(offset).expect("few closures")
        );
    }
}

// ----- Pass 1: module-level names ----------------------------------------------------------

/// A module-level value that `$name` can refer to.
#[derive(Clone, Copy)]
enum ValueItem {
    Const(ConstId),
    Global(GlobalId),
}

/// The names of a type's members, known before the type is lowered.
#[derive(Default)]
struct TypeNames {
    /// Each field of a struct: its name, and whether it has a default value.
    fields: Vec<(String, bool)>,
    /// Each variant of an enum: its name and its fields' names.
    variants: Vec<(String, Vec<String>)>,
    /// Methods and associated functions.
    functions: HashMap<String, FnId>,
}

/// The declaration of a user-defined type.
enum TypeDecl {
    Struct(ast::StructDecl),
    Enum(ast::EnumDecl),
}

impl TypeDecl {
    fn name(&self) -> Option<ast::Name> {
        match self {
            Self::Struct(decl) => decl.name(),
            Self::Enum(decl) => decl.name(),
        }
    }

    fn keyword_span(&self) -> Span {
        match self {
            Self::Struct(decl) => decl.keyword_span(),
            Self::Enum(decl) => decl.keyword_span(),
        }
    }

    fn functions(&self) -> Vec<ast::FnDecl> {
        match self {
            Self::Struct(decl) => decl.functions(),
            Self::Enum(decl) => decl.functions(),
        }
    }

    fn generic_params(&self) -> Option<ast::GenericParamList> {
        match self {
            Self::Struct(decl) => decl.generic_params(),
            Self::Enum(decl) => decl.generic_params(),
        }
    }

    fn kind(&self) -> AdtKind {
        match self {
            Self::Struct(_) => AdtKind::Struct,
            Self::Enum(_) => AdtKind::Enum,
        }
    }

    fn impl_list(&self) -> Vec<ast::Type> {
        match self {
            Self::Struct(decl) => decl.impl_list(),
            Self::Enum(decl) => decl.impl_list(),
        }
    }
}

/// The names of a trait's members, known before the trait is lowered.
#[derive(Default)]
struct TraitNames {
    functions: HashMap<String, FnId>,
    /// The user-defined traits listed in the trait's `impl=`.
    supertraits: Vec<TraitId>,
}

/// The declarations of one module.
struct Collected {
    /// The module.
    module: ModuleId,
    /// The items it declares.
    items: Items,
    /// Names of top-level `:local` variables of a script, for better error messages.
    script_locals: HashSet<String>,
    uses: Vec<ast::UseDecl>,
    types: Vec<(TypeId, TypeDecl)>,
    traits: Vec<(TraitId, ast::TraitDecl)>,
    impls: Vec<(ImplId, ast::ImplDecl)>,
    tests: Vec<ast::TestDecl>,
    /// Functions, with the type or trait that declares them.
    fns: Vec<(FnId, ast::FnDecl, Option<FnOwner>)>,
    consts: Vec<(ConstId, ast::ConstDecl)>,
    globals: Vec<(GlobalId, ast::GlobalDecl)>,
    script: Vec<ast::Stmt>,
}

fn not_supported(feature: &str, milestone: &str, span: Span) -> Diagnostic {
    Diagnostic::error(
        codes::NOT_SUPPORTED_YET,
        format!(
            "{feature} {} not supported yet",
            if feature.ends_with('s') { "are" } else { "is" }
        ),
        span,
    )
    .with_help(format!("planned for milestone {milestone} of the compiler"))
}

fn spanned_name(name: Option<ast::Name>, fallback: Span) -> Spanned<String> {
    name.map_or_else(
        || Spanned {
            value: String::new(),
            span: fallback,
        },
        |n| Spanned {
            value: n.text(),
            span: n.span(),
        },
    )
}

fn collect(
    source: &Source<'_>,
    module: &mut Module,
    shared: &mut Shared,
    diagnostics: &mut Vec<Diagnostic>,
) -> Collected {
    let mut collected = Collected {
        module: source.id,
        items: Items::default(),
        script_locals: HashSet::new(),
        uses: Vec::new(),
        types: Vec::new(),
        traits: Vec::new(),
        impls: Vec::new(),
        tests: Vec::new(),
        fns: Vec::new(),
        consts: Vec::new(),
        globals: Vec::new(),
        script: Vec::new(),
    };
    let stmts: Vec<ast::Stmt> = source.file.stmts().collect();
    // Types and traits first, so that functions can be checked against their names.
    for stmt in &stmts {
        match stmt {
            ast::Stmt::StructDecl(decl) => {
                collected.add_type(TypeDecl::Struct(decl.clone()), module, shared, diagnostics);
            }
            ast::Stmt::EnumDecl(decl) => {
                collected.add_type(TypeDecl::Enum(decl.clone()), module, shared, diagnostics);
            }
            ast::Stmt::TraitDecl(decl) => {
                collected.add_trait(decl.clone(), module, shared, diagnostics);
            }
            _ => {}
        }
    }
    let mut value_spans = HashMap::new();
    for stmt in stmts {
        match stmt {
            ast::Stmt::StructDecl(_) | ast::Stmt::EnumDecl(_) | ast::Stmt::TraitDecl(_) => {}
            ast::Stmt::UseDecl(decl) => collected.uses.push(decl),
            ast::Stmt::FnDecl(decl) => {
                collected.add_fn(decl, FnKind::Declared, module, diagnostics);
            }
            ast::Stmt::ExternDecl(decl) => collected.add_extern(&decl, module, diagnostics),
            ast::Stmt::ImplDecl(decl) => collected.add_impl(decl, module, shared, diagnostics),
            ast::Stmt::TestDecl(decl) => collected.tests.push(decl),
            ast::Stmt::ConstDecl(decl) => {
                let Some(name) = new_value_name(decl.name(), &mut value_spans, diagnostics) else {
                    continue;
                };
                let id = module.consts.alloc(ConstItem {
                    name: name.clone(),
                    module: source.id,
                    ty: None,
                    body: Body::default(),
                    init: None,
                });
                collected
                    .items
                    .values
                    .insert(name.value, ValueItem::Const(id));
                collected.consts.push((id, decl));
            }
            ast::Stmt::GlobalDecl(decl) => {
                let Some(name) = new_value_name(decl.name(), &mut value_spans, diagnostics) else {
                    continue;
                };
                let id = module.globals.alloc(GlobalItem {
                    name: name.clone(),
                    module: source.id,
                    ty: None,
                    body: Body::default(),
                    init: None,
                });
                collected
                    .items
                    .values
                    .insert(name.value, ValueItem::Global(id));
                collected.globals.push((id, decl));
            }
            other => {
                if let Some((feature, milestone, span)) = unsupported_form(&other) {
                    diagnostics.push(not_supported(feature, milestone, span));
                    continue;
                }
                if !source.entry {
                    diagnostics.push(
                        Diagnostic::error(
                            codes::STATEMENTS_OUTSIDE_ENTRY,
                            "only the program's entry module can have top-level statements",
                            other.span(),
                        )
                        .with_help("other modules contain only declarations; move the statement into a function"),
                    );
                    continue;
                }
                if let ast::Stmt::LocalDecl(decl) = &other
                    && let Some(name) = decl.name()
                {
                    collected.script_locals.insert(name.text());
                }
                collected.script.push(other);
            }
        }
    }
    collected
}

impl Collected {
    /// `:impl Type {...}`: functions of a built-in type, declared by the standard library.
    fn add_impl(
        &mut self,
        decl: ast::ImplDecl,
        module: &mut Module,
        shared: &mut Shared,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let package = module.modules[self.module].path.first();
        if package.map(String::as_str) != Some(crate::STD_PACKAGE) {
            diagnostics.push(
                Diagnostic::error(
                    codes::INVALID_IMPL,
                    "only the standard library can declare functions of built-in types",
                    decl.keyword_span(),
                )
                .with_help("declare the functions of a struct or enum in its body"),
            );
            return;
        }
        if let Some(first) = decl.impl_list().first() {
            diagnostics.push(
                Diagnostic::error(
                    codes::NOT_SUPPORTED_YET,
                    "implementing traits with `:impl` is not supported in v0",
                    first.span(),
                )
                .with_help("planned for v1 of the language (spec section 15)"),
            );
        }
        let head = decl.self_type().as_ref().and_then(written_head);
        let id = module.impls.alloc(ImplDef {
            module: self.module,
            generics: Generics::default(),
            self_ty: Spanned {
                value: Ty::Error,
                span: decl.keyword_span(),
            },
            head,
            functions: Vec::new(),
        });
        for function in decl.functions() {
            let Some(fn_name) = function.name() else {
                diagnostics.push(Diagnostic::error(
                    codes::INCOMPLETE_DECLARATION,
                    "a function of an `:impl` needs a name",
                    function.keyword_span(),
                ));
                continue;
            };
            let fn_name = spanned_name(Some(fn_name), function.keyword_span());
            if let Some(head) = head
                && let Some(&previous) = shared.impl_fns.get(&(head, fn_name.value.clone()))
            {
                let previous_span = module.functions[previous].name.span;
                diagnostics.push(duplicate_item(&fn_name, previous_span, "function"));
                continue;
            }
            let owner = Some(FnOwner::Impl(id));
            let fn_id = module
                .functions
                .alloc(placeholder_fn(fn_name.clone(), owner, self.module));
            if let Some(head) = head {
                shared.impl_fns.insert((head, fn_name.value.clone()), fn_id);
            }
            module.impls[id].functions.push((fn_name.value, fn_id));
            self.fns.push((fn_id, function, owner));
        }
        self.impls.push((id, decl));
    }

    /// `:extern lib="..." {...}`: functions of the runtime, declared by the standard library,
    /// or foreign functions.
    fn add_extern(
        &mut self,
        decl: &ast::ExternDecl,
        module: &mut Module,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let lib = decl.lib().as_ref().and_then(plain_string);
        if lib.as_deref() != Some("pika") {
            diagnostics.push(not_supported(
                "foreign functions",
                "M8",
                decl.keyword_span(),
            ));
            return;
        }
        let package = module.modules[self.module].path.first();
        if package.map(String::as_str) != Some(crate::STD_PACKAGE) {
            diagnostics.push(Diagnostic::error(
                codes::INVALID_RUNTIME_FUNCTION,
                "only the standard library can declare functions of the Pika runtime",
                decl.keyword_span(),
            ));
            return;
        }
        for function in decl.functions() {
            self.add_fn(function, FnKind::Runtime, module, diagnostics);
        }
    }

    fn add_fn(
        &mut self,
        decl: ast::FnDecl,
        kind: FnKind,
        module: &mut Module,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(name) = decl.name() else {
            diagnostics.push(Diagnostic::error(
                codes::INCOMPLETE_DECLARATION,
                "a function declared at the top level needs a name",
                decl.keyword_span(),
            ));
            return;
        };
        let name = spanned_name(Some(name), decl.keyword_span());
        if RESERVED_COMMANDS.contains(&name.value.as_str()) {
            diagnostics.push(
                Diagnostic::error(
                    codes::RESERVED_NAME,
                    format!(
                        "`{}` is a built-in command and cannot name a function",
                        name.value
                    ),
                    name.span,
                )
                .with_help("choose another name"),
            );
            return;
        }
        if let Some(&previous) = self.items.fns.get(&name.value) {
            let previous_span = module.functions[previous].name.span;
            diagnostics.push(duplicate_item(&name, previous_span, "function"));
            return;
        }
        let mut placeholder = placeholder_fn(name.clone(), None, self.module);
        placeholder.kind = kind;
        let id = module.functions.alloc(placeholder);
        self.items.fns.insert(name.value, id);
        self.fns.push((id, decl, None));
    }

    /// Registers a type, the names of its members and its functions, before anything is
    /// lowered.
    /// Returns true if `name` can name a new type or trait; otherwise reports why not.
    fn is_new_type_name(
        &self,
        name: &Spanned<String>,
        module: &Module,
        diagnostics: &mut Vec<Diagnostic>,
    ) -> bool {
        let builtin_type = Ty::from_name(&name.value).is_some()
            || matches!(
                name.value.as_str(),
                "List" | "Map" | "Set" | "Box" | "Option" | "Error" | "Ptr" | "Self"
            );
        let builtin_trait = BuiltinTrait::from_name(&name.value).is_some()
            || PreludeTrait::ALL.iter().any(|p| p.name() == name.value)
            || name.value == "Contains";
        if builtin_type || builtin_trait {
            diagnostics.push(Diagnostic::error(
                codes::DUPLICATE_ITEM,
                format!(
                    "`{}` is a built-in {} and cannot be redeclared",
                    name.value,
                    if builtin_type { "type" } else { "trait" }
                ),
                name.span,
            ));
            return false;
        }
        let previous = match (
            self.items.types.get(&name.value),
            self.items.traits.get(&name.value),
        ) {
            (Some(&(id, _)), _) => Some(module.types[id].name.span),
            (None, Some(&id)) => Some(module.traits[id].name.span),
            (None, None) => None,
        };
        if let Some(previous) = previous {
            diagnostics.push(duplicate_item(name, previous, "type or trait"));
            return false;
        }
        true
    }

    /// Registers a trait and its functions, before anything is lowered.
    fn add_trait(
        &mut self,
        decl: ast::TraitDecl,
        module: &mut Module,
        shared: &mut Shared,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(name) = decl.name() else {
            return;
        };
        let name = spanned_name(Some(name), decl.keyword_span());
        if !self.is_new_type_name(&name, module, diagnostics) {
            return;
        }
        if let Some(list) = decl.generic_params() {
            diagnostics.push(
                Diagnostic::error(
                    codes::NOT_SUPPORTED_YET,
                    "traits with type parameters are not supported in v0",
                    list.span(),
                )
                .with_help("planned for v1 of the language (spec section 15)"),
            );
        }
        let generic = decl.generic_params().is_some();
        let id = module.traits.alloc(TraitDef {
            name: name.clone(),
            module: self.module,
            prelude: None,
            supertraits: Vec::new(),
            functions: Vec::new(),
        });
        self.items.traits.insert(name.value.clone(), id);
        let mut members = TraitNames::default();
        // The functions of a generic trait are not lowered: they would refer to its
        // parameters, which are not supported.
        for function in decl.functions().into_iter().filter(|_| !generic) {
            let Some(fn_name) = function.name() else {
                diagnostics.push(Diagnostic::error(
                    codes::INCOMPLETE_DECLARATION,
                    "a function of a trait needs a name",
                    function.keyword_span(),
                ));
                continue;
            };
            let fn_name = spanned_name(Some(fn_name), function.keyword_span());
            if let Some(&previous) = members.functions.get(&fn_name.value) {
                let previous_span = module.functions[previous].name.span;
                diagnostics.push(duplicate_item(&fn_name, previous_span, "function"));
                continue;
            }
            let owner = Some(FnOwner::Trait(id));
            let fn_id = module
                .functions
                .alloc(placeholder_fn(fn_name.clone(), owner, self.module));
            members.functions.insert(fn_name.value.clone(), fn_id);
            module.traits[id].functions.push((fn_name.value, fn_id));
            self.fns.push((fn_id, function, owner));
        }
        shared.trait_members.insert(id, members);
        self.traits.push((id, decl));
    }

    fn add_type(
        &mut self,
        decl: TypeDecl,
        module: &mut Module,
        shared: &mut Shared,
        diagnostics: &mut Vec<Diagnostic>,
    ) {
        let Some(name) = decl.name() else {
            return;
        };
        let name = spanned_name(Some(name), decl.keyword_span());
        if !self.is_new_type_name(&name, module, diagnostics) {
            return;
        }
        let kind = decl.kind();
        let id = module.types.alloc(TypeDef {
            name: name.clone(),
            module: self.module,
            ty: Ty::Error,
            kind,
            generics: Generics::default(),
            fields: Vec::new(),
            variants: Vec::new(),
            derives: Derives::default(),
            traits: Vec::new(),
            functions: Vec::new(),
            body: Body::default(),
        });
        let index = id.into_raw().into_u32();
        // A generic type is written with its parameters as its arguments, inside its body.
        let params: Vec<Ty> = decl
            .generic_params()
            .map(|list| list.names())
            .unwrap_or_default()
            .iter()
            .enumerate()
            .map(|(index, name)| param_ty(index, name))
            .collect();
        let ty = Ty::Adt(Adt::intern(kind, index, &name.value, &params));
        module.types[id].ty = ty;
        self.items.types.insert(name.value.clone(), (id, ty));
        shared.type_tys.insert(id, ty);
        let mut members = data_member_names(&decl);
        for function in decl.functions() {
            let Some(fn_name) = function.name() else {
                diagnostics.push(Diagnostic::error(
                    codes::INCOMPLETE_DECLARATION,
                    "a method needs a name",
                    function.keyword_span(),
                ));
                continue;
            };
            let fn_name = spanned_name(Some(fn_name), function.keyword_span());
            if let Some(&previous) = members.functions.get(&fn_name.value) {
                let previous_span = module.functions[previous].name.span;
                diagnostics.push(duplicate_item(&fn_name, previous_span, "method"));
                continue;
            }
            if members.variants.iter().any(|(v, _)| *v == fn_name.value) {
                diagnostics.push(Diagnostic::error(
                    codes::DUPLICATE_ITEM,
                    format!(
                        "`{}` is both a variant and a function of `{}`",
                        fn_name.value, name.value
                    ),
                    fn_name.span,
                ));
                continue;
            }
            let owner = Some(FnOwner::Type(id));
            let fn_id = module
                .functions
                .alloc(placeholder_fn(fn_name.clone(), owner, self.module));
            members.functions.insert(fn_name.value.clone(), fn_id);
            module.types[id].functions.push((fn_name.value, fn_id));
            self.fns.push((fn_id, function, owner));
        }
        shared.members.insert(id, members);
        self.types.push((id, decl));
    }
}

/// Declares the prelude in a module of its own: the `Error` type and the prelude traits, whose
/// functions the language calls for operators and destruction. Returns their names.
fn add_prelude(module: &mut Module, shared: &mut Shared) -> Items {
    let prelude = module.modules.alloc(ModuleDef { path: Vec::new() });
    let mut items = Items::default();
    add_error_type(module, shared, &mut items, prelude);
    for which in PreludeTrait::ALL {
        let span = Span::default();
        let id = module.traits.alloc(TraitDef {
            name: Spanned {
                value: which.name().to_owned(),
                span,
            },
            module: prelude,
            prelude: Some(which),
            supertraits: Vec::new(),
            functions: Vec::new(),
        });
        let function = module.functions.alloc(prelude_function(id, which, prelude));
        module.traits[id]
            .functions
            .push((which.function().to_owned(), function));
        items.traits.insert(which.name().to_owned(), id);
        shared.trait_members.insert(
            id,
            TraitNames {
                functions: HashMap::from([(which.function().to_owned(), function)]),
                supertraits: Vec::new(),
            },
        );
    }
    items
}

/// Declares the `Error` type of `raises` functions, as if written:
///
/// ```text
/// :struct Error impl=Clone,Display {
///     message:String
///     source:Box<Error>?=none
///     file:String=""
///     line:u32=0
///     column:u32=0
///     trace:List<String>={}
/// }
/// ```
///
/// Its values display as their message.
fn add_error_type(module: &mut Module, shared: &mut Shared, items: &mut Items, prelude: ModuleId) {
    let span = Span::default();
    let spanned = |value: &str| Spanned {
        value: value.to_owned(),
        span,
    };
    let id = module.types.alloc(TypeDef {
        name: spanned("Error"),
        module: prelude,
        ty: Ty::Error,
        kind: AdtKind::Struct,
        generics: Generics::default(),
        fields: Vec::new(),
        variants: Vec::new(),
        derives: Derives {
            clone: Some(span),
            display: Some(span),
            ..Derives::default()
        },
        traits: Vec::new(),
        functions: Vec::new(),
        body: Body::default(),
    });
    let ty = Ty::Adt(Adt::intern(
        AdtKind::Struct,
        id.into_raw().into_u32(),
        "Error",
        &[],
    ));
    let def = &mut module.types[id];
    def.ty = ty;
    let mut default = |expr: Expr| {
        let id = def.body.exprs.alloc(expr);
        def.body.expr_spans.insert(id, span);
        Some(id)
    };
    let zero = || {
        Expr::Literal(Literal::Int {
            value: 0,
            negative: false,
        })
    };
    let fields = [
        ("message", Ty::String, None),
        ("source", Ty::option(Ty::boxed(ty)), default(Expr::None)),
        ("file", Ty::String, default(Expr::String(Vec::new()))),
        ("line", Ty::Int(IntTy::U32), default(zero())),
        ("column", Ty::Int(IntTy::U32), default(zero())),
        (
            "trace",
            Ty::list(Ty::String),
            default(Expr::Collection {
                declared: None,
                elements: Vec::new(),
            }),
        ),
    ];
    def.fields = fields
        .into_iter()
        .map(|(name, field_ty, default)| FieldDef {
            name: spanned(name),
            ty: Spanned {
                value: field_ty,
                span,
            },
            default,
        })
        .collect();
    let members = TypeNames {
        fields: def
            .fields
            .iter()
            .map(|f| (f.name.value.clone(), f.default.is_some()))
            .collect(),
        ..TypeNames::default()
    };
    items.types.insert("Error".to_owned(), (id, ty));
    shared.members.insert(id, members);
    shared.type_tys.insert(id, ty);
    module.error_type = Some(id);
}

/// The head of a type as written, as `List` in `List<T>`, if it is a built-in type.
fn written_head(ty: &ast::Type) -> Option<TypeHead> {
    match ty {
        ast::Type::Path(path) => TypeHead::named(&path.name()?.text()),
        ast::Type::Option(_) => Some(TypeHead::Option),
        ast::Type::Fn(_) => None,
    }
}

/// The text of a string literal without interpolation, as in `lib="pika"`.
fn plain_string(expr: &ast::Expr) -> Option<String> {
    let ast::Expr::StringLit(string) = expr else {
        return None;
    };
    let mut text = String::new();
    for part in string.parts() {
        let ast::StringPart::Text(token) = part else {
            return None;
        };
        text.push_str(&literal::unescape(token.text()).0);
    }
    Some(text)
}

/// The name of a new module-level constant or global, or `None` if it is missing or already
/// taken (which is reported).
fn new_value_name(
    name: Option<ast::Name>,
    taken: &mut HashMap<String, Span>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Spanned<String>> {
    let name = name?;
    let name = Spanned {
        value: name.text(),
        span: name.span(),
    };
    if let Some(previous) = taken.get(&name.value) {
        diagnostics.push(duplicate_item(&name, *previous, "value"));
        return None;
    }
    taken.insert(name.value.clone(), name.span);
    Some(name)
}

/// A statement form implemented by a later milestone: its description, the milestone, and
/// where it starts.
fn unsupported_form(stmt: &ast::Stmt) -> Option<(&'static str, &'static str, Span)> {
    Some(match stmt {
        ast::Stmt::ExternDecl(d) => ("foreign functions", "M8", d.keyword_span()),
        ast::Stmt::UnsafeBlock(s) => ("`:unsafe`", "M8", s.keyword_span()),
        _ => return None,
    })
}

/// For a declaration that belongs at the top level of a file, written in a function: why it
/// cannot be there, and where it starts.
fn top_level_only(stmt: &ast::Stmt) -> Option<(&'static str, Span)> {
    Some(match stmt {
        ast::Stmt::FnDecl(decl) if decl.name().is_some() => (
            "functions can only be declared at the top level of a file",
            decl.keyword_span(),
        ),
        ast::Stmt::FnDecl(decl) => (
            "a function at the start of a statement needs a name",
            decl.keyword_span(),
        ),
        ast::Stmt::StructDecl(decl) => (
            "structs can only be declared at the top level of a file",
            decl.keyword_span(),
        ),
        ast::Stmt::EnumDecl(decl) => (
            "enums can only be declared at the top level of a file",
            decl.keyword_span(),
        ),
        ast::Stmt::TraitDecl(decl) => (
            "traits can only be declared at the top level of a file",
            decl.keyword_span(),
        ),
        ast::Stmt::UseDecl(decl) => (
            "`:use` can only be written at the top level of a file",
            decl.keyword_span(),
        ),
        ast::Stmt::ImplDecl(decl) => (
            "`:impl` can only be written at the top level of a file",
            decl.keyword_span(),
        ),
        ast::Stmt::TestDecl(decl) => (
            "`:test` can only be written at the top level of a file",
            decl.keyword_span(),
        ),
        _ => return None,
    })
}

fn duplicate_item(name: &Spanned<String>, previous: Span, what: &str) -> Diagnostic {
    Diagnostic::error(
        codes::DUPLICATE_ITEM,
        format!("the {what} `{}` is declared more than once", name.value),
        name.span,
    )
    .with_secondary(previous, "first declared here")
}

/// The names of the fields of a struct, or of the variants of an enum and their fields.
fn data_member_names(decl: &TypeDecl) -> TypeNames {
    let mut members = TypeNames::default();
    match decl {
        TypeDecl::Struct(decl) => {
            for field in decl.fields() {
                let field_name = field.name().map(|n| n.text()).unwrap_or_default();
                members
                    .fields
                    .push((field_name, field.default_value().is_some()));
            }
        }
        TypeDecl::Enum(decl) => {
            for variant in decl.variants() {
                let variant_name = variant.name().map(|n| n.text()).unwrap_or_default();
                let field_names = variant
                    .fields()
                    .iter()
                    .map(|f| f.name().map(|n| n.text()).unwrap_or_default())
                    .collect();
                members.variants.push((variant_name, field_names));
            }
        }
    }
    members
}

/// The required function of a prelude trait: `:fn add self other:Self -> Self` for the
/// operators that take two values, `:fn neg self -> Self`, and `:fn drop mut self`.
fn prelude_function(id: TraitId, which: PreludeTrait, module: ModuleId) -> Function {
    let span = Span::default();
    let self_ty = param_ty(0, "Self");
    let mut body = Body::default();
    let mut param = |name: &str, convention: Convention| {
        let local = body.locals.alloc(Local {
            name: Spanned {
                value: name.to_owned(),
                span,
            },
            kind: LocalKind::Param(convention),
            ty: Some(Spanned {
                value: self_ty,
                span,
            }),
        });
        Param {
            local,
            is_self: name == "self",
            convention,
            ty: Spanned {
                value: self_ty,
                span,
            },
            default: None,
        }
    };
    let (params, ret) = match which {
        PreludeTrait::Drop => (vec![param("self", Convention::Mut)], Ty::Nothing),
        PreludeTrait::Neg => (vec![param("self", Convention::Read)], self_ty),
        _ => (
            vec![
                param("self", Convention::Read),
                param("other", Convention::Read),
            ],
            self_ty,
        ),
    };
    Function {
        name: Spanned {
            value: which.function().to_owned(),
            span,
        },
        module,
        kind: FnKind::Declared,
        owner: Some(FnOwner::Trait(id)),
        generics: Generics {
            params: vec![GenericParam {
                name: Spanned {
                    value: "Self".to_owned(),
                    span,
                },
                ty: self_ty,
                bounds: vec![Spanned {
                    value: Bound::Trait(id),
                    span,
                }],
            }],
        },
        own_generics: 1,
        params,
        ret: Spanned { value: ret, span },
        raises: false,
        captures: Vec::new(),
        has_body: false,
        body,
        root: Block::default(),
    }
}

/// The variable that holds the captured variable `outer`, named `name`, in a closure's
/// `body`, adding it to the closure's `captures` on first use, at `span`.
fn capture_in(
    body: &mut Body,
    captures: &mut Vec<Capture>,
    name: &str,
    outer: LocalId,
    span: Span,
) -> LocalId {
    if let Some(capture) = captures.iter().find(|c| c.outer == outer) {
        return capture.inner;
    }
    let inner = body.locals.alloc(Local {
        name: Spanned {
            value: name.to_owned(),
            span,
        },
        kind: LocalKind::Captured,
        ty: None,
    });
    captures.push(Capture { outer, inner });
    inner
}

fn placeholder_fn(name: Spanned<String>, owner: Option<FnOwner>, module: ModuleId) -> Function {
    let span = name.span;
    Function {
        name,
        module,
        kind: FnKind::Declared,
        owner,
        generics: Generics::default(),
        own_generics: 0,
        params: Vec::new(),
        ret: Spanned {
            value: Ty::Nothing,
            span,
        },
        raises: false,
        captures: Vec::new(),
        has_body: true,
        body: Body::default(),
        root: Block::default(),
    }
}

// ----- Types -------------------------------------------------------------------------------

/// What type names refer to where a type is written.
struct TypeScope<'a> {
    /// The module's types and traits.
    names: &'a ModuleNames,
    /// The type `Self` names, inside a struct.
    self_ty: Option<Ty>,
    /// The type parameters in scope, by name.
    params: Vec<(String, Ty)>,
}

/// The type of the type parameter at `index`, named `name`.
fn param_ty(index: usize, name: &str) -> Ty {
    Ty::Param(TyParam::intern(
        u32::try_from(index).expect("few type parameters"),
        name,
    ))
}

/// The type that declares a function, for its methods.
struct Owner {
    kind: FnOwner,
    /// The type `Self` names: the type, or a trait's `Self` parameter.
    ty: Ty,
    /// The type parameters the owner gives its functions.
    generics: Generics,
}

impl Owner {
    fn new(kind: FnOwner, module: &Module) -> Self {
        match kind {
            FnOwner::Type(id) => Self {
                kind,
                ty: module.types[id].ty,
                generics: module.types[id].generics.clone(),
            },
            FnOwner::Impl(id) => Self {
                kind,
                ty: module.impls[id].self_ty.value,
                generics: module.impls[id].generics.clone(),
            },
            FnOwner::Trait(id) => {
                let name = &module.traits[id].name;
                let ty = param_ty(0, "Self");
                Self {
                    kind,
                    ty,
                    generics: Generics {
                        params: vec![GenericParam {
                            name: Spanned {
                                value: "Self".to_owned(),
                                span: name.span,
                            },
                            ty,
                            bounds: vec![Spanned {
                                value: Bound::Trait(id),
                                span: name.span,
                            }],
                        }],
                    },
                }
            }
        }
    }
}

/// Lowers the `impl=` list of a trait: the traits it requires.
fn lower_trait(
    decl: &ast::TraitDecl,
    id: TraitId,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut supertraits: Vec<Spanned<Bound>> = Vec::new();
    for item in decl.impl_list() {
        let Some(bound) = lower_bound(&item, names, diagnostics) else {
            continue;
        };
        if bound.value == Bound::Trait(id) {
            diagnostics.push(Diagnostic::error(
                codes::INVALID_BOUND,
                "a trait cannot require itself",
                bound.span,
            ));
            continue;
        }
        if supertraits.iter().any(|b| b.value == bound.value) {
            diagnostics.push(Diagnostic::error(
                codes::DUPLICATE_ITEM,
                "this trait is listed more than once",
                bound.span,
            ));
            continue;
        }
        supertraits.push(bound);
    }
    module.traits[id].supertraits = supertraits;
}

/// A trait named in a bound or an `impl=` list: built-in or user-defined.
fn lower_bound(
    item: &ast::Type,
    names: &ModuleNames,
    diagnostics: &mut Vec<Diagnostic>,
) -> Option<Spanned<Bound>> {
    let span = item.span();
    if let ast::Type::Path(path) = item
        && path.generic_args().is_none()
        && let Some(module_path) = path.path()
    {
        return match names.path_trait(&module_path) {
            Ok(id) => Some(Spanned {
                value: Bound::Trait(id),
                span,
            }),
            Err(diagnostic) => {
                diagnostics.push(*diagnostic);
                None
            }
        };
    }
    let name = match item {
        ast::Type::Path(path) if path.generic_args().is_none() => path.name().map(|n| n.text()),
        _ => None,
    };
    let Some(name) = name else {
        diagnostics.push(Diagnostic::error(
            codes::INVALID_BOUND,
            "expected the name of a trait",
            span,
        ));
        return None;
    };
    if let Some(builtin) = BuiltinTrait::from_name(&name) {
        return Some(Spanned {
            value: Bound::Builtin(builtin),
            span,
        });
    }
    if let Some(&id) = names.items.traits.get(&name) {
        return Some(Spanned {
            value: Bound::Trait(id),
            span,
        });
    }
    let diagnostic = if names.items.types.contains_key(&name) || Ty::from_name(&name).is_some() {
        Diagnostic::error(
            codes::INVALID_BOUND,
            format!("`{name}` is a type, not a trait"),
            span,
        )
    } else {
        Diagnostic::error(
            codes::INVALID_BOUND,
            format!("unknown trait `{name}`"),
            span,
        )
    };
    diagnostics.push(diagnostic);
    None
}

/// Lowers the type parameters of a declaration, numbered from `start`; `inherited` are the
/// parameters already in scope (those of the owner type).
fn lower_generics(
    list: Option<&ast::GenericParamList>,
    inherited: &[GenericParam],
    scope: &TypeScope<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Vec<GenericParam> {
    let Some(list) = list else {
        return Vec::new();
    };
    let mut params: Vec<GenericParam> = Vec::new();
    for param in list.params() {
        let name = spanned_name(param.name(), param.span());
        let taken = inherited
            .iter()
            .chain(&params)
            .find(|p| p.name.value == name.value);
        if let Some(previous) = taken {
            diagnostics.push(duplicate_item(&name, previous.name.span, "type parameter"));
            continue;
        }
        if scope.names.items.types.contains_key(&name.value) || Ty::from_name(&name.value).is_some()
        {
            diagnostics.push(Diagnostic::error(
                codes::DUPLICATE_ITEM,
                format!("the type parameter `{}` has the name of a type", name.value),
                name.span,
            ));
        }
        let mut bounds: Vec<Spanned<Bound>> = Vec::new();
        for bound in param.bounds() {
            if let Some(bound) = lower_bound(&bound, scope.names, diagnostics) {
                bounds.push(bound);
            }
        }
        let index = inherited.len() + params.len();
        params.push(GenericParam {
            ty: param_ty(index, &name.value),
            name,
            bounds,
        });
    }
    params
}

/// Lowers a type.
fn lower_type(
    ty: &ast::Type,
    scope: &TypeScope<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Spanned<Ty> {
    let span = ty.span();
    let value = match ty {
        ast::Type::Path(path) => lower_path_type(path, scope, diagnostics),
        ast::Type::Fn(function) => {
            let params: Vec<Ty> = function
                .params()
                .iter()
                .map(|param| lower_type(param, scope, diagnostics).value)
                .collect();
            let ret = function.ret().map_or(Ty::Nothing, |ret| {
                lower_type(&ret, scope, diagnostics).value
            });
            Ty::function(&params, ret, function.is_raises())
        }
        ast::Type::Option(option) => match option.inner() {
            Some(inner) => Ty::option(lower_type(&inner, scope, diagnostics).value),
            None => Ty::Error,
        },
    };
    Spanned { value, span }
}

fn lower_path_type(
    path: &ast::PathType,
    scope: &TypeScope<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Ty {
    let span = path.span();
    let Some(name) = path.name() else {
        let Some(module_path) = path.path() else {
            return Ty::Error;
        };
        return match scope.names.path_type(&module_path) {
            Ok((_, Ty::Adt(adt))) => {
                let written = module_path.syntax().text().to_string();
                adt_with_args(path, &written, adt, scope, diagnostics)
            }
            Ok((_, ty)) => ty,
            Err(diagnostic) => {
                diagnostics.push(*diagnostic);
                Ty::Error
            }
        };
    };
    let name = name.text();
    if let Some(&(_, param)) = scope.params.iter().find(|(n, _)| *n == name) {
        if let Some(args) = path.generic_args() {
            diagnostics.push(Diagnostic::error(
                codes::UNKNOWN_TYPE,
                format!("the type parameter `{name}` does not take type arguments"),
                args.span(),
            ));
        }
        return param;
    }
    let resolved = match name.as_str() {
        "Self" => scope.self_ty,
        _ => Ty::from_name(&name).or_else(|| scope.names.items.types.get(&name).map(|&(_, ty)| ty)),
    };
    if let ("Option" | "Box" | "List" | "Set" | "Map", None) = (name.as_str(), resolved) {
        return lower_builtin_generic(path, &name, scope, diagnostics);
    }
    if let (Some(Ty::Adt(adt)), false) = (resolved, name == "Self") {
        return adt_with_args(path, &name, adt, scope, diagnostics);
    }
    if let Some(args) = path.generic_args()
        && resolved.is_some()
    {
        diagnostics.push(Diagnostic::error(
            codes::UNKNOWN_TYPE,
            format!("`{name}` does not take type arguments"),
            args.span(),
        ));
        return Ty::Error;
    }
    if resolved.is_none() && scope.names.items.traits.contains_key(&name) {
        diagnostics.push(
            Diagnostic::error(
                codes::UNKNOWN_TYPE,
                format!("`{name}` is a trait, not a type"),
                span,
            )
            .with_help(format!(
                "use a type parameter bounded by it, as in `:fn f<T: {name}> x:T`; trait objects are planned for v1"
            )),
        );
        return Ty::Error;
    }
    resolved.unwrap_or_else(|| unknown_type(&name, span, diagnostics))
}

/// The user-defined type `adt`, written `name`, with the type arguments written in `path`.
fn adt_with_args(
    path: &ast::PathType,
    name: &str,
    adt: &'static Adt,
    scope: &TypeScope<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Ty {
    let args: Vec<Ty> = path
        .generic_args()
        .map(|a| a.types())
        .unwrap_or_default()
        .iter()
        .map(|arg| lower_type(arg, scope, diagnostics).value)
        .collect();
    match type_args_error(name, adt.args.len(), args.len(), path.span()) {
        None => Ty::adt(adt, &args),
        Some(diagnostic) => {
            diagnostics.push(diagnostic);
            Ty::Error
        }
    }
}

/// A built-in generic type: `Option`, `Box`, `List`, `Set` or `Map`.
fn lower_builtin_generic(
    path: &ast::PathType,
    name: &str,
    scope: &TypeScope<'_>,
    diagnostics: &mut Vec<Diagnostic>,
) -> Ty {
    let span = path.span();
    let args = path.generic_args().map(|a| a.types()).unwrap_or_default();
    let args: Vec<Ty> = args
        .iter()
        .map(|arg| lower_type(arg, scope, diagnostics).value)
        .collect();
    match (name, args.as_slice()) {
        ("Option", &[inner]) => Ty::option(inner),
        ("Box", &[inner]) => Ty::boxed(inner),
        ("List", &[element]) => Ty::list(element),
        ("Set", &[element]) => Ty::set(element),
        ("Map", &[key, value]) => Ty::map_of(key, value),
        ("Map", _) => {
            diagnostics.push(Diagnostic::error(
                codes::UNKNOWN_TYPE,
                "`Map` takes two type arguments, as in `Map<String, i64>`",
                span,
            ));
            Ty::Error
        }
        _ => {
            diagnostics.push(Diagnostic::error(
                codes::UNKNOWN_TYPE,
                format!("`{name}` takes one type argument, as in `{name}<String>`"),
                span,
            ));
            Ty::Error
        }
    }
}

/// Reports a type name that does not name a type (yet).
fn unknown_type(name: &str, span: Span, diagnostics: &mut Vec<Diagnostic>) -> Ty {
    let (feature, milestone) = match name {
        "Self" => {
            diagnostics.push(Diagnostic::error(
                codes::UNKNOWN_TYPE,
                "`Self` can only be used inside a struct or an enum",
                span,
            ));
            return Ty::Error;
        }
        "Ptr" => ("raw pointers", "M8"),
        _ => {
            diagnostics.push(Diagnostic::error(
                codes::UNKNOWN_TYPE,
                format!("unknown type `{name}`"),
                span,
            ));
            return Ty::Error;
        }
    };
    diagnostics.push(not_supported(feature, milestone, span));
    Ty::Error
}

/// The error for the wrong number of type arguments given to the type `name`, which has
/// `expected` type parameters, if the number is wrong.
fn type_args_error(name: &str, expected: usize, given: usize, span: Span) -> Option<Diagnostic> {
    if given == expected {
        return None;
    }
    let message = if expected == 0 {
        format!("`{name}` does not take type arguments")
    } else if given == 0 {
        format!(
            "`{name}` needs {expected} type argument{}",
            if expected == 1 { "" } else { "s" }
        )
    } else {
        format!(
            "`{name}` takes {expected} type argument{}, but {given} were given",
            if expected == 1 { "" } else { "s" }
        )
    };
    let placeholders: Vec<&str> = (0..expected).map(|_| "...").collect();
    let mut diagnostic = Diagnostic::error(codes::UNKNOWN_TYPE, message, span);
    if expected > 0 {
        diagnostic = diagnostic.with_help(format!(
            "write them after the name: `{name}<{}>`",
            placeholders.join(", ")
        ));
    }
    Some(diagnostic)
}

// ----- Pass 2: functions and initializers --------------------------------------------------

/// Lowers a function. `owner` is the struct that declares it, with the struct's type.
///
/// `id` is the function's id, and the closures in it get ids from `first_closure` on: they are
/// returned, to be added to the module in order.
fn lower_fn(
    decl: &ast::FnDecl,
    names: &ModuleNames,
    owner: Option<&Owner>,
    (id, kind, first_closure): (FnId, FnKind, u32),
    diagnostics: &mut Vec<Diagnostic>,
) -> (Function, Vec<Function>) {
    let name = spanned_name(decl.name(), decl.keyword_span());
    let inherited: Vec<GenericParam> = owner.map(|o| o.generics.params.clone()).unwrap_or_default();
    let mut params_in_scope: Vec<(String, Ty)> = inherited
        .iter()
        .map(|p| (p.name.value.clone(), p.ty))
        .collect();
    let own = {
        let scope = TypeScope {
            names,
            self_ty: owner.map(|o| o.ty),
            params: params_in_scope.clone(),
        };
        lower_generics(
            decl.generic_params().as_ref(),
            &inherited,
            &scope,
            diagnostics,
        )
    };
    params_in_scope.extend(own.iter().map(|p| (p.name.value.clone(), p.ty)));
    let own_generics = inherited.len();
    let generics = Generics {
        params: inherited.into_iter().chain(own).collect(),
    };

    let mut ctx = BodyCtx::new(names, diagnostics, true);
    ctx.params = params_in_scope;
    ctx.self_ty = owner.map(|o| o.ty);
    ctx.trait_bounds = trait_bounds(names, &generics);
    ctx.fn_id = Some(id);
    ctx.generics = generics.clone();
    ctx.first_closure = first_closure;
    ctx.push_scope();
    let params: Vec<Param> = decl
        .params()
        .into_iter()
        .enumerate()
        .filter_map(|(index, param)| lower_param(&mut ctx, &param, index, owner.map(|o| o.ty)))
        .collect();

    let ret = match decl.ret_type() {
        Some(ty) => {
            let scope = ctx.type_scope();
            lower_type(&ty, &scope, ctx.diagnostics)
        }
        None => Spanned {
            value: Ty::Nothing,
            span: name.span,
        },
    };

    let in_trait = matches!(
        owner,
        Some(Owner {
            kind: FnOwner::Trait(_),
            ..
        })
    );
    if in_trait {
        reject_trait_defaults(&mut ctx, &params);
    }
    let runtime = kind == FnKind::Runtime;
    if runtime {
        reject_runtime_parts(&mut ctx, decl);
    }
    let has_body = (decl.body().is_some() || !in_trait) && !runtime;
    let root = if runtime {
        Block::default()
    } else if let Some(block) = decl.body() {
        ctx.lower_block(&block)
    } else if in_trait {
        // A required function: each implementing type defines it.
        Block::default()
    } else {
        ctx.diagnostics.push(
            Diagnostic::error(
                codes::INCOMPLETE_DECLARATION,
                format!("function `{}` has no body", name.value),
                name.span,
            )
            .with_help("add a body: `do={ ... }`"),
        );
        Block::default()
    };
    ctx.pop_scope();
    let closures = ctx.take_closures();
    let body = ctx.finish();

    let function = Function {
        name,
        module: names.module,
        kind,
        owner: owner.map(|o| o.kind),
        generics,
        own_generics,
        params,
        ret,
        raises: decl.is_raises(),
        captures: Vec::new(),
        has_body,
        body,
        root,
    };
    (function, closures)
}

/// The user-defined traits each type parameter is bounded by, with the traits they require.
fn trait_bounds(names: &ModuleNames, generics: &Generics) -> HashMap<String, Vec<TraitId>> {
    generics
        .params
        .iter()
        .map(|param| {
            let traits = param.bounds.iter().filter_map(|b| match b.value {
                Bound::Trait(id) => Some(id),
                Bound::Builtin(_) => None,
            });
            (param.name.value.clone(), names.shared.trait_closure(traits))
        })
        .collect()
}

/// Reports the parts that a function of the runtime cannot have: a body, type parameters and
/// default values, which the runtime would not know.
fn reject_runtime_parts(ctx: &mut BodyCtx<'_>, decl: &ast::FnDecl) {
    let parts = [
        decl.body().map(|body| ("a body", body.span())),
        decl.generic_params()
            .map(|list| ("type parameters", list.span())),
    ];
    for (part, span) in parts.into_iter().flatten() {
        ctx.diagnostics.push(Diagnostic::error(
            codes::INVALID_RUNTIME_FUNCTION,
            format!("a function of the runtime cannot have {part}"),
            span,
        ));
    }
    for param in decl.params() {
        if let Some(default) = param.default_value() {
            ctx.diagnostics.push(Diagnostic::error(
                codes::INVALID_RUNTIME_FUNCTION,
                "a function of the runtime cannot have default values",
                default.span(),
            ));
        }
    }
}

/// Reports default values of the parameters of a trait's function.
fn reject_trait_defaults(ctx: &mut BodyCtx<'_>, params: &[Param]) {
    for param in params {
        if let Some(default) = param.default {
            let span = ctx.body.expr_span(default);
            ctx.diagnostics.push(
                Diagnostic::error(
                    codes::INCOMPLETE_DECLARATION,
                    "the parameters of a trait's functions cannot have default values",
                    span,
                )
                .with_help("callers through the trait could not know the default"),
            );
        }
    }
}

/// Reports the default value of a parameter or field (`what`) whose type has type parameters:
/// a default is a constant, which cannot have every type. Returns true if it is reported.
fn reject_generic_default(ctx: &mut BodyCtx<'_>, ty: Ty, default: ExprId, what: &str) -> bool {
    if !ty.has_params() {
        return false;
    }
    ctx.diagnostics.push(
        Diagnostic::error(
            codes::INCOMPLETE_DECLARATION,
            format!("a {what} whose type has type parameters cannot have a default value"),
            ctx.body.expr_span(default),
        )
        .with_help("a default value is a constant, so it cannot have every type"),
    );
    true
}

/// Lowers parameter `index` of a function declared by `owner`, if any. A misplaced `self` is
/// reported and dropped.
fn lower_param(
    ctx: &mut BodyCtx<'_>,
    param: &ast::Param,
    index: usize,
    owner: Option<Ty>,
) -> Option<Param> {
    if param.is_self() {
        let Some(self_ty) = owner else {
            ctx.diagnostics.push(Diagnostic::error(
                codes::INVALID_SELF,
                "`self` can only be a parameter of a method declared in a struct",
                param.span(),
            ));
            return None;
        };
        if index > 0 {
            ctx.diagnostics.push(Diagnostic::error(
                codes::INVALID_SELF,
                "`self` must be the first parameter",
                param.span(),
            ));
        }
        let ty = Spanned {
            value: self_ty,
            span: param.span(),
        };
        let name = Spanned {
            value: "self".to_owned(),
            span: param.span(),
        };
        let local = ctx.declare(name, LocalKind::Param(param.convention()), Some(ty.clone()));
        return Some(Param {
            local,
            is_self: true,
            convention: param.convention(),
            ty,
            default: None,
        });
    }
    if param.convention() == ast::Convention::Mut
        && let Some(value) = param.default_value()
    {
        ctx.diagnostics.push(
            Diagnostic::error(
                codes::INCOMPLETE_DECLARATION,
                "a `mut` parameter cannot have a default value",
                value.span(),
            )
            .with_help("the caller must pass a variable for the function to modify"),
        );
    }
    let default = param.default_value().map(|value| {
        let id = ctx.lower_expr(&value);
        ctx.require_const(id, "default values");
        id
    });
    let ty = param.ty().map_or_else(
        || Spanned {
            value: Ty::Error,
            span: param.span(),
        },
        |t| {
            let scope = ctx.type_scope();
            lower_type(&t, &scope, ctx.diagnostics)
        },
    );
    let default =
        default.filter(|&default| !reject_generic_default(ctx, ty.value, default, "parameter"));
    let local = ctx.declare(
        spanned_name(param.name(), param.span()),
        LocalKind::Param(param.convention()),
        Some(ty.clone()),
    );
    Some(Param {
        local,
        is_self: false,
        convention: param.convention(),
        ty,
        default,
    })
}

/// Lowers the fields and `impl=` list of a struct.
fn lower_struct(
    decl: &ast::StructDecl,
    id: TypeId,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let self_ty = module.types[id].ty;
    let generics = {
        let scope = TypeScope {
            names,
            self_ty: Some(self_ty),
            params: Vec::new(),
        };
        lower_generics(decl.generic_params().as_ref(), &[], &scope, diagnostics)
    };
    let scope = TypeScope {
        names,
        self_ty: Some(self_ty),
        params: generics
            .iter()
            .map(|p| (p.name.value.clone(), p.ty))
            .collect(),
    };
    module.types[id].generics = Generics { params: generics };

    let mut ctx = BodyCtx::new(names, diagnostics, false);
    let mut fields: Vec<FieldDef> = Vec::new();
    for field in decl.fields() {
        let name = spanned_name(field.name(), field.span());
        if let Some(previous) = fields.iter().find(|f| f.name.value == name.value) {
            let previous_span = previous.name.span;
            ctx.diagnostics
                .push(duplicate_item(&name, previous_span, "field"));
            continue;
        }
        let ty = field.ty().map_or_else(
            || Spanned {
                value: Ty::Error,
                span: field.span(),
            },
            |t| lower_type(&t, &scope, ctx.diagnostics),
        );
        let default = field
            .default_value()
            .map(|value| {
                let id = ctx.lower_expr(&value);
                ctx.require_const(id, "default values of fields");
                id
            })
            .filter(|&default| !reject_generic_default(&mut ctx, ty.value, default, "field"));
        fields.push(FieldDef { name, ty, default });
    }
    let body = ctx.finish();

    let (derives, traits) = lower_derives(&decl.impl_list(), names, diagnostics);

    let strukt = &mut module.types[id];
    strukt.fields = fields;
    strukt.derives = derives;
    strukt.traits = traits;
    strukt.body = body;
}

/// The traits listed in the `impl=` of a type: the built-in ones it derives, and the
/// user-defined ones it implements.
fn lower_derives(
    impl_list: &[ast::Type],
    names: &ModuleNames,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Derives, Vec<Spanned<TraitId>>) {
    let mut derives = Derives::default();
    let mut traits: Vec<Spanned<TraitId>> = Vec::new();
    for item in impl_list {
        let Some(bound) = lower_bound(item, names, diagnostics) else {
            continue;
        };
        let span = bound.span;
        let listed = match bound.value {
            Bound::Trait(id) => {
                let listed = traits.iter().any(|t| t.value == id);
                if !listed {
                    traits.push(Spanned { value: id, span });
                }
                listed
            }
            Bound::Builtin(builtin) => {
                let slot = match builtin {
                    BuiltinTrait::Copy => &mut derives.copy,
                    BuiltinTrait::Clone => &mut derives.clone,
                    BuiltinTrait::Eq => &mut derives.eq,
                    BuiltinTrait::Ord => &mut derives.ord,
                    BuiltinTrait::Display => &mut derives.display,
                    BuiltinTrait::Hash => &mut derives.hash,
                    BuiltinTrait::Default => &mut derives.default,
                };
                slot.replace(span).is_some()
            }
        };
        if listed {
            diagnostics.push(Diagnostic::error(
                codes::DUPLICATE_ITEM,
                "this trait is listed more than once",
                span,
            ));
        }
    }
    (derives, traits)
}

/// Lowers the variants and `impl=` list of an enum.
fn lower_enum(
    decl: &ast::EnumDecl,
    id: TypeId,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let self_ty = module.types[id].ty;
    let generics = {
        let scope = TypeScope {
            names,
            self_ty: Some(self_ty),
            params: Vec::new(),
        };
        lower_generics(decl.generic_params().as_ref(), &[], &scope, diagnostics)
    };
    let scope = TypeScope {
        names,
        self_ty: Some(self_ty),
        params: generics
            .iter()
            .map(|p| (p.name.value.clone(), p.ty))
            .collect(),
    };
    module.types[id].generics = Generics { params: generics };
    let mut variants: Vec<VariantDef> = Vec::new();
    for variant in decl.variants() {
        let name = spanned_name(variant.name(), variant.span());
        if let Some(previous) = variants.iter().find(|v| v.name.value == name.value) {
            diagnostics.push(duplicate_item(&name, previous.name.span, "variant"));
            continue;
        }
        let mut fields: Vec<FieldDef> = Vec::new();
        for field in variant.fields() {
            let field_name = spanned_name(field.name(), field.span());
            if let Some(previous) = fields.iter().find(|f| f.name.value == field_name.value) {
                diagnostics.push(duplicate_item(&field_name, previous.name.span, "field"));
                continue;
            }
            let ty = field.ty().map_or_else(
                || Spanned {
                    value: Ty::Error,
                    span: field.span(),
                },
                |t| lower_type(&t, &scope, diagnostics),
            );
            fields.push(FieldDef {
                name: field_name,
                ty,
                default: None,
            });
        }
        variants.push(VariantDef { name, fields });
    }
    if variants.is_empty() {
        diagnostics.push(
            Diagnostic::error(
                codes::INCOMPLETE_DECLARATION,
                format!("enum `{}` has no variants", module.types[id].name.value),
                module.types[id].name.span,
            )
            .with_help("an enum lists its variants: `:enum Color { red; green }`"),
        );
    }
    let (derives, traits) = lower_derives(&decl.impl_list(), names, diagnostics);
    let def = &mut module.types[id];
    def.variants = variants;
    def.derives = derives;
    def.traits = traits;
}

/// Lowers the top-level statements of a script, as function `id`; its closures get ids from
/// `id + 1` on, and are returned.
fn lower_statements(
    stmts: &[ast::Stmt],
    names: &ModuleNames,
    Implicit {
        id,
        kind,
        name,
        raises,
    }: Implicit,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Function, Vec<Function>) {
    let span = name.span;
    let mut ctx = BodyCtx::new(names, diagnostics, true);
    ctx.fn_id = Some(id);
    ctx.first_closure = id.into_raw().into_u32() + 1;
    ctx.push_scope();
    let stmts = stmts.iter().filter_map(|s| ctx.lower_stmt(s)).collect();
    ctx.pop_scope();
    let closures = ctx.take_closures();
    let body = ctx.finish();
    let function = Function {
        name,
        module: names.module,
        kind,
        owner: None,
        generics: Generics::default(),
        own_generics: 0,
        params: Vec::new(),
        ret: Spanned {
            value: Ty::Nothing,
            span,
        },
        raises,
        captures: Vec::new(),
        has_body: true,
        body,
        root: Block { stmts, span: None },
    };
    (function, closures)
}

/// A function made of statements that are not written in a `:fn`: the implicit `main` of a
/// script, or the body of a test.
struct Implicit {
    /// The function's id; its closures get the ids that follow.
    id: FnId,
    kind: FnKind,
    name: Spanned<String>,
    raises: bool,
}

/// The tests of a module: `:test "name" do={...}`, each a function that raises the errors it
/// does not catch.
fn lower_tests(
    collected: &Collected,
    names: &ModuleNames,
    module: &mut Module,
    diagnostics: &mut Vec<Diagnostic>,
) {
    let mut seen: HashMap<String, Span> = HashMap::new();
    for decl in &collected.tests {
        let Some(name) = decl.name() else {
            // Reported by the parser.
            continue;
        };
        let span = name.span();
        let Some(text) = plain_string(&ast::Expr::StringLit(name)) else {
            diagnostics.push(Diagnostic::error(
                codes::INCOMPLETE_DECLARATION,
                "the name of a test is a string without interpolation",
                span,
            ));
            continue;
        };
        let name = Spanned { value: text, span };
        if let Some(&previous) = seen.get(&name.value) {
            diagnostics.push(duplicate_item(&name, previous, "test"));
            continue;
        }
        seen.insert(name.value.clone(), span);
        let stmts: Vec<ast::Stmt> = decl.body().map(|b| b.stmts().collect()).unwrap_or_default();
        let id = FnId::from_raw(la_arena::RawIdx::from_u32(next_fn_id(module)));
        let implicit = Implicit {
            id,
            kind: FnKind::Test,
            name: name.clone(),
            raises: true,
        };
        let (function, closures) = lower_statements(&stmts, names, implicit, diagnostics);
        let allocated = module.functions.alloc(function);
        debug_assert_eq!(allocated, id);
        add_closures(module, id.into_raw().into_u32() + 1, closures);
        module.tests.push(TestDef {
            name,
            module: collected.module,
            function: id,
        });
    }
}

/// Lowers the initializer of a module-level constant or global.
fn lower_initializer(
    value: Option<ast::Expr>,
    names: &ModuleNames,
    diagnostics: &mut Vec<Diagnostic>,
) -> (Body, Option<ExprId>) {
    let mut ctx = BodyCtx::new(names, diagnostics, false);
    let init = value.map(|v| {
        let id = ctx.lower_expr(&v);
        ctx.require_const(id, "module-level initializers");
        id
    });
    (ctx.finish(), init)
}

/// The expression of a literal. Malformed literals were reported by the lexer and lower to
/// `Missing`.
fn lower_literal(lit: &ast::Literal) -> Expr {
    let negative = lit.is_negative();
    let Some(kind) = lit.kind() else {
        return Expr::Missing;
    };
    // Malformed literals were reported by the lexer and lower to `Missing`.
    let literal = match kind {
        LiteralKind::Int(text) => literal::parse_int(&text)
            .ok()
            .map(|value| Literal::Int { value, negative }),
        LiteralKind::Float(text) => literal::parse_float(&text)
            .ok()
            .map(|v| Literal::Float(if negative { -v } else { v })),
        LiteralKind::Duration(text) => literal::parse_duration(&text)
            .ok()
            .map(|v| Literal::Duration(if negative { -v } else { v })),
        LiteralKind::Char(text) => literal::parse_char(&text).ok().map(Literal::Char),
        LiteralKind::Bool(value) => Some(Literal::Bool(value)),
        LiteralKind::RawString(text) => {
            return Expr::String(vec![StringPart::Text(literal::raw_string_value(&text))]);
        }
        LiteralKind::None => return Expr::None,
    };
    literal.map_or(Expr::Missing, Expr::Literal)
}

// ----- Bodies ------------------------------------------------------------------------------

/// The path of a path expression, with its type arguments, and the path alone.
fn path_parts(path: &ast::PathExpr) -> Option<(ast::PathType, ast::Path)> {
    let path_type = path.path_type()?;
    let module_path = path_type.path()?;
    Some((path_type, module_path))
}

struct BodyCtx<'a> {
    names: &'a ModuleNames,
    diagnostics: &'a mut Vec<Diagnostic>,
    body: Body,
    scopes: Vec<HashMap<String, LocalId>>,
    loop_depth: u32,
    /// False for module-level initializers, which have no scope for local variables.
    in_function: bool,
    /// The type parameters in scope, by name.
    params: Vec<(String, Ty)>,
    /// The type `Self` names, in the methods of a struct.
    self_ty: Option<Ty>,
    /// The user-defined traits each type parameter in scope is bounded by, with the traits
    /// they require.
    trait_bounds: HashMap<String, Vec<TraitId>>,
    /// The function whose body is being lowered; closures get their own.
    fn_id: Option<FnId>,
    /// The type parameters of the function, which its closures share.
    generics: Generics,
    /// The variables of enclosing functions that the closure being lowered captures.
    captures: Vec<Capture>,
    /// While lowering a closure: the bodies of the functions that enclose it, outermost first.
    enclosing: Vec<Frame>,
    /// The closures created in the function, in the order of their ids, which start at
    /// `first_closure`. A closure's slot is filled when it has been lowered.
    closures: Vec<Option<Function>>,
    first_closure: u32,
}

/// The state of a function body whose lowering is suspended to lower a closure in it.
struct Frame {
    body: Body,
    scopes: Vec<HashMap<String, LocalId>>,
    loop_depth: u32,
    captures: Vec<Capture>,
    fn_id: Option<FnId>,
}

impl<'a> BodyCtx<'a> {
    fn new(
        names: &'a ModuleNames,
        diagnostics: &'a mut Vec<Diagnostic>,
        in_function: bool,
    ) -> Self {
        Self {
            names,
            diagnostics,
            body: Body::default(),
            scopes: Vec::new(),
            loop_depth: 0,
            in_function,
            params: Vec::new(),
            self_ty: None,
            trait_bounds: HashMap::new(),
            fn_id: None,
            generics: Generics::default(),
            captures: Vec::new(),
            enclosing: Vec::new(),
            closures: Vec::new(),
            first_closure: 0,
        }
    }

    /// The closures lowered, in order.
    fn take_closures(&mut self) -> Vec<Function> {
        std::mem::take(&mut self.closures)
            .into_iter()
            .map(|closure| closure.expect("every closure is lowered"))
            .collect()
    }

    /// The variable `name`: a local of this body, or a variable of an enclosing function,
    /// which the closures in between capture.
    fn resolve_local(&mut self, name: &str, span: Span) -> Option<LocalId> {
        if let Some(local) = self.lookup_local(name) {
            return Some(local);
        }
        let depth = self
            .enclosing
            .iter()
            .rposition(|frame| frame.scopes.iter().any(|scope| scope.contains_key(name)))?;
        let mut outer = self.enclosing[depth]
            .scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())?;
        // Each closure from there inwards captures the variable of the one around it.
        for index in depth + 1..self.enclosing.len() {
            let frame = &mut self.enclosing[index];
            outer = capture_in(&mut frame.body, &mut frame.captures, name, outer, span);
        }
        Some(capture_in(
            &mut self.body,
            &mut self.captures,
            name,
            outer,
            span,
        ))
    }

    /// `[:fn params -> Ret do={...}]`: lowers the closure's body as a function of its own,
    /// and returns the expression that creates it.
    fn lower_closure(&mut self, decl: &ast::FnDecl) -> Expr {
        let index = self.closures.len();
        self.closures.push(None);
        let id = FnId::from_raw(la_arena::RawIdx::from_u32(
            self.first_closure + u32::try_from(index).expect("fewer than 2^32 closures"),
        ));
        let span = decl.keyword_span();
        if let Some(name) = decl.name() {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::INCOMPLETE_DECLARATION,
                    "a function value has no name",
                    name.span(),
                )
                .with_help("write `[:fn x:i64 -> i64 do={ ... }]`, and store it in a variable"),
            );
        }
        if let Some(list) = decl.generic_params() {
            self.diagnostics.push(Diagnostic::error(
                codes::INCOMPLETE_DECLARATION,
                "a function value cannot have type parameters of its own",
                list.span(),
            ));
        }
        let parent = self.fn_id;
        self.enclosing.push(Frame {
            body: std::mem::take(&mut self.body),
            scopes: std::mem::take(&mut self.scopes),
            loop_depth: std::mem::replace(&mut self.loop_depth, 0),
            captures: std::mem::take(&mut self.captures),
            fn_id: self.fn_id.replace(id),
        });
        self.push_scope();
        let params: Vec<Param> = decl
            .params()
            .into_iter()
            .enumerate()
            .filter_map(|(position, param)| lower_param(self, &param, position, None))
            .collect();
        self.reject_closure_params(&params);
        let ret = match decl.ret_type() {
            Some(ty) => {
                let scope = self.type_scope();
                lower_type(&ty, &scope, self.diagnostics)
            }
            None => Spanned {
                value: Ty::Nothing,
                span,
            },
        };
        let root = if let Some(block) = decl.body() {
            self.lower_block(&block)
        } else {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::INCOMPLETE_DECLARATION,
                    "a function value needs a body",
                    span,
                )
                .with_help("add a body: `do={ ... }`"),
            );
            Block::default()
        };
        self.pop_scope();
        let frame = self.enclosing.pop().expect("pushed above");
        let body = std::mem::replace(&mut self.body, frame.body);
        let captures = std::mem::replace(&mut self.captures, frame.captures);
        self.scopes = frame.scopes;
        self.loop_depth = frame.loop_depth;
        self.fn_id = frame.fn_id;
        self.closures[index] = Some(Function {
            name: Spanned {
                value: "closure".to_owned(),
                span,
            },
            module: self.names.module,
            kind: FnKind::Closure(parent.expect("closures are created in functions")),
            owner: None,
            generics: self.generics.clone(),
            own_generics: self.generics.params.len(),
            params,
            ret,
            raises: decl.is_raises(),
            captures,
            has_body: true,
            body,
            root,
        });
        Expr::Closure(id)
    }

    /// Reports the parameters of a closure that are not borrowed for reading, or have default
    /// values: a function value is called with every argument, each borrowed.
    fn reject_closure_params(&mut self, params: &[Param]) {
        for param in params {
            let span = self.body.locals[param.local].name.span;
            if param.convention != Convention::Read {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::INVALID_FORM_ARGUMENTS,
                        "the parameters of a function value are borrowed for reading",
                        span,
                    )
                    .with_help("remove `mut` or `owned`; return a new value instead"),
                );
            }
            if let Some(default) = param.default {
                let default_span = self.body.expr_span(default);
                self.diagnostics.push(Diagnostic::error(
                    codes::INVALID_FORM_ARGUMENTS,
                    "the parameters of a function value have no default values",
                    default_span,
                ));
            }
        }
    }

    fn type_scope(&self) -> TypeScope<'a> {
        TypeScope {
            names: self.names,
            self_ty: self.self_ty,
            params: self.params.clone(),
        }
    }

    fn finish(self) -> Body {
        self.body
    }

    fn push_scope(&mut self) {
        self.scopes.push(HashMap::new());
    }

    fn pop_scope(&mut self) {
        self.scopes.pop();
    }

    fn declare(
        &mut self,
        name: Spanned<String>,
        kind: LocalKind,
        ty: Option<Spanned<Ty>>,
    ) -> LocalId {
        let scope = self.scopes.last().expect("a scope is open");
        if let Some(&previous) = scope.get(&name.value) {
            let previous_span = self.body.locals[previous].name.span;
            self.diagnostics.push(
                Diagnostic::error(
                    codes::DUPLICATE_LOCAL,
                    format!("`{}` is already declared in this scope", name.value),
                    name.span,
                )
                .with_secondary(previous_span, "first declared here")
                .with_help("use `:set` to change its value, or choose another name"),
            );
        }
        let key = name.value.clone();
        let id = self.body.locals.alloc(Local { name, kind, ty });
        self.scopes
            .last_mut()
            .expect("a scope is open")
            .insert(key, id);
        id
    }

    fn lookup_local(&self, name: &str) -> Option<LocalId> {
        self.scopes
            .iter()
            .rev()
            .find_map(|scope| scope.get(name).copied())
    }

    fn alloc_expr(&mut self, expr: Expr, span: Span) -> ExprId {
        let id = self.body.exprs.alloc(expr);
        self.body.expr_spans.insert(id, span);
        id
    }

    fn alloc_stmt(&mut self, stmt: Stmt, span: Span) -> StmtId {
        let id = self.body.stmts.alloc(stmt);
        self.body.stmt_spans.insert(id, span);
        id
    }

    fn lower_block(&mut self, block: &ast::Block) -> Block {
        self.push_scope();
        let stmts = block.stmts().filter_map(|s| self.lower_stmt(&s)).collect();
        self.pop_scope();
        Block {
            stmts,
            span: Some(block.span()),
        }
    }

    fn lower_optional_block(&mut self, block: Option<ast::Block>) -> Block {
        block.map(|b| self.lower_block(&b)).unwrap_or_default()
    }

    /// Reports named arguments given more than once to a built-in form.
    fn check_duplicate_args(&mut self, node: &pika_syntax::SyntaxNode) {
        let mut seen = HashSet::new();
        for arg in ast::named_args(node) {
            if let Some(name) = arg.name_text()
                && !seen.insert(name.clone())
            {
                self.diagnostics.push(Diagnostic::error(
                    codes::DUPLICATE_ARGUMENT,
                    format!("argument `{name}` is given more than once"),
                    arg.span(),
                ));
            }
        }
    }

    fn lower_stmt(&mut self, stmt: &ast::Stmt) -> Option<StmtId> {
        if let Some((feature, milestone, span)) = unsupported_form(stmt) {
            self.diagnostics
                .push(not_supported(feature, milestone, span));
            return None;
        }
        if let Some((message, span)) = top_level_only(stmt) {
            self.diagnostics
                .push(Diagnostic::error(codes::NESTED_FUNCTION, message, span));
            return None;
        }
        let span = stmt.span();
        let lowered = match stmt {
            ast::Stmt::Call(call) => Stmt::Expr(self.lower_call(call)),
            ast::Stmt::LocalDecl(decl) => self.lower_let(
                LocalKind::Var,
                spanned_name(decl.name(), decl.keyword_span()),
                decl.ty(),
                decl.value(),
            ),
            ast::Stmt::ConstDecl(decl) => self.lower_let(
                LocalKind::Const,
                spanned_name(decl.name(), decl.keyword_span()),
                decl.ty(),
                decl.value(),
            ),
            ast::Stmt::GlobalDecl(decl) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::GLOBAL_IN_FUNCTION,
                        "`:global` can only be used at the top level of a file",
                        decl.keyword_span(),
                    )
                    .with_help("use `:local` for a variable of this function"),
                );
                return None;
            }
            ast::Stmt::SetStmt(set) => {
                let value = self.lower_optional_expr(set.value(), span);
                let target = self.lower_set_target(set);
                Stmt::Set { target, value }
            }
            ast::Stmt::IfStmt(if_stmt) => return Some(self.lower_if(if_stmt)),
            ast::Stmt::WhileStmt(while_stmt) => {
                self.check_duplicate_args(while_stmt.syntax());
                let cond = self.lower_optional_expr(while_stmt.condition(), span);
                let body = self.lower_loop_body(while_stmt.body());
                Stmt::While { cond, body }
            }
            ast::Stmt::DoWhileStmt(do_while) => {
                self.check_duplicate_args(do_while.syntax());
                let body = self.lower_loop_body(do_while.body());
                let cond = self.lower_optional_expr(do_while.condition(), span);
                Stmt::DoWhile { body, cond }
            }
            ast::Stmt::ForStmt(for_stmt) => self.lower_for(for_stmt),
            ast::Stmt::Block(block) => Stmt::Block(self.lower_block(block)),
            ast::Stmt::MatchStmt(match_stmt) => self.lower_match(match_stmt),
            ast::Stmt::ForeachStmt(foreach) => self.lower_foreach(foreach),
            ast::Stmt::OnErrorStmt(onerror) => self.lower_onerror(onerror),
            ast::Stmt::FnDecl(_)
            | ast::Stmt::StructDecl(_)
            | ast::Stmt::EnumDecl(_)
            | ast::Stmt::TraitDecl(_)
            | ast::Stmt::ImplDecl(_)
            | ast::Stmt::TestDecl(_)
            | ast::Stmt::UseDecl(_) => unreachable!("reported by `top_level_only`"),
            ast::Stmt::UnsafeBlock(_) | ast::Stmt::ExternDecl(_) => {
                unreachable!("reported by `unsupported_form`")
            }
        };
        Some(self.alloc_stmt(lowered, span))
    }

    /// `:foreach x in=... do={...}`, with an index or key: `:foreach i,x ...`.
    fn lower_foreach(&mut self, foreach: &ast::ForeachStmt) -> Stmt {
        self.check_duplicate_args(foreach.syntax());
        let span = foreach.span();
        let collection = self.lower_optional_expr(foreach.collection(), span);
        let names = foreach.names();
        let (key_name, value_name) = match names.as_slice() {
            [value] => (None, Some(value.clone())),
            [key, value] => (Some(key.clone()), Some(value.clone())),
            _ => (None, None),
        };
        self.push_scope();
        let key = key_name
            .map(|name| self.declare(spanned_name(Some(name), span), LocalKind::LoopVar, None));
        let value = self.declare(
            spanned_name(value_name, foreach.keyword_span()),
            LocalKind::Element {
                mutable: foreach.is_mut(),
            },
            None,
        );
        let body = self.lower_loop_body(foreach.body());
        self.pop_scope();
        Stmt::Foreach {
            key,
            value,
            collection,
            body,
        }
    }

    /// `:onerror e in={...} do={...}`: the error is in scope in the handler only.
    fn lower_onerror(&mut self, onerror: &ast::OnErrorStmt) -> Stmt {
        self.check_duplicate_args(onerror.syntax());
        let body = self.lower_optional_block(onerror.body());
        self.push_scope();
        let error = self.declare(
            spanned_name(onerror.name(), onerror.keyword_span()),
            LocalKind::Caught,
            None,
        );
        let handler = self.lower_optional_block(onerror.handler());
        self.pop_scope();
        Stmt::OnError {
            error,
            body,
            handler,
        }
    }

    /// `:error value` or `:error value source=$cause`.
    fn lower_raise(&mut self, call: &ast::Call) -> ExprId {
        let span = call.span();
        let args = self.lower_call_args(call);
        let mut value = None;
        let mut source = None;
        for arg in args {
            match &arg.name {
                None if value.is_none() => value = Some(arg.value),
                Some(name) if name.value == "source" && source.is_none() => {
                    source = Some(arg.value);
                }
                _ => {
                    let arg_span = self.body.expr_span(arg.value);
                    self.diagnostics.push(
                        Diagnostic::error(
                            codes::INVALID_FORM_ARGUMENTS,
                            "`:error` takes a message or an error, and an optional `source=`",
                            arg_span,
                        )
                        .with_help("write `:error \"message\"`, or `:error \"message\" source=$e`"),
                    );
                }
            }
        }
        let value = value.unwrap_or_else(|| {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::INVALID_FORM_ARGUMENTS,
                    "`:error` needs a message or an error",
                    span,
                )
                .with_help("write `:error \"message\"`"),
            );
            self.alloc_expr(Expr::Missing, span)
        });
        self.alloc_expr(Expr::Raise { value, source }, span)
    }

    /// `:match scrutinee { pattern if=(guard) do={...} ... }`
    fn lower_match(&mut self, match_stmt: &ast::MatchStmt) -> Stmt {
        let span = match_stmt.span();
        let scrutinee = self.lower_optional_expr(match_stmt.scrutinee(), span);
        let mut arms = Vec::new();
        for arm in match_stmt.arms() {
            self.check_duplicate_args(arm.syntax());
            // The pattern's bindings are visible in the guard and the body.
            self.push_scope();
            let pat = match arm.pattern() {
                Some(pattern) => self.lower_pattern(&pattern),
                None => self.alloc_pat(Pat::Invalid(Vec::new()), arm.span()),
            };
            let guard = arm.guard().map(|guard| self.lower_expr(&guard));
            let body = self.lower_optional_block(arm.body());
            self.pop_scope();
            arms.push(MatchArm { pat, guard, body });
        }
        Stmt::Match { scrutinee, arms }
    }

    fn alloc_pat(&mut self, pat: Pat, span: Span) -> PatId {
        let id = self.body.pats.alloc(pat);
        self.body.pat_spans.insert(id, span);
        id
    }

    fn lower_pattern(&mut self, pattern: &ast::Pattern) -> PatId {
        let span = pattern.span();
        let pat = match pattern {
            ast::Pattern::Wildcard(_) => Pat::Wildcard,
            ast::Pattern::None(_) => Pat::None,
            ast::Pattern::Paren(paren) => {
                return match paren.pattern() {
                    Some(inner) => self.lower_pattern(&inner),
                    None => self.alloc_pat(Pat::Invalid(Vec::new()), span),
                };
            }
            ast::Pattern::Some(some) => {
                let inner = match some.pattern() {
                    Some(inner) => self.lower_pattern(&inner),
                    None => self.alloc_pat(Pat::Invalid(Vec::new()), span),
                };
                Pat::Some(inner)
            }
            ast::Pattern::Binding(binding) => {
                let name = spanned_name(binding.name(), span);
                Pat::Binding(self.declare(name, LocalKind::Binding, None))
            }
            ast::Pattern::Literal(literal) => self.lower_literal_pattern(literal),
            ast::Pattern::Variant(variant) => self.lower_variant_pattern(variant),
        };
        self.alloc_pat(pat, span)
    }

    fn lower_literal_pattern(&mut self, pattern: &ast::LiteralPat) -> Pat {
        let span = pattern.span();
        if let Some(string) = pattern.string() {
            let mut text = String::new();
            for part in string.parts() {
                match part {
                    ast::StringPart::Text(token) => {
                        text.push_str(&literal::unescape(token.text()).0);
                    }
                    ast::StringPart::Var(_) | ast::StringPart::Interpolation(_) => {
                        self.diagnostics.push(Diagnostic::error(
                            codes::INVALID_PATTERN,
                            "a string pattern cannot interpolate values",
                            string.span(),
                        ).with_help("match on the value with a binding and compare it in a guard: `s if=($s = ...)`"));
                        return Pat::Invalid(Vec::new());
                    }
                }
            }
            return Pat::String(text);
        }
        let Some((kind, negative)) = pattern.literal() else {
            return Pat::Invalid(Vec::new());
        };
        // Malformed literals were reported by the lexer.
        let literal = match kind {
            LiteralKind::Int(text) => literal::parse_int(&text)
                .ok()
                .map(|value| Literal::Int { value, negative }),
            LiteralKind::Char(text) => literal::parse_char(&text).ok().map(Literal::Char),
            LiteralKind::Bool(value) => Some(Literal::Bool(value)),
            LiteralKind::RawString(text) => return Pat::String(literal::raw_string_value(&text)),
            LiteralKind::Float(_) | LiteralKind::Duration(_) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::INVALID_PATTERN,
                        "floats and durations cannot be used as patterns",
                        span,
                    )
                    .with_help("bind the value and compare it in a guard: `x if=($x < 1.5)`"),
                );
                None
            }
            LiteralKind::None => return Pat::None,
        };
        literal.map_or(Pat::Invalid(Vec::new()), Pat::Literal)
    }

    /// `Enum->variant p1 p2 ...`
    fn lower_variant_pattern(&mut self, pattern: &ast::VariantPat) -> Pat {
        let span = pattern.span();
        let sub_patterns: Vec<PatId> = pattern
            .fields()
            .iter()
            .map(|p| self.lower_pattern(p))
            .collect();
        let (Some(type_name), Some(name)) = (pattern.ty(), pattern.variant()) else {
            return Pat::Invalid(sub_patterns);
        };
        let Some((ty, type_args)) = self.type_named(&type_name) else {
            return Pat::Invalid(sub_patterns);
        };
        let type_text = type_name.syntax().text().to_string();
        let name_text = name.text();
        let Some(variant) = self.variant_index(ty, &name_text) else {
            self.diagnostics.push(Diagnostic::error(
                codes::FIELD_MISMATCH,
                format!("`{type_text}` has no variant named `{name_text}`"),
                name.span(),
            ));
            return Pat::Invalid(sub_patterns);
        };
        let field_count = self.names.shared.members[&ty].variants[variant].1.len();
        if sub_patterns.len() != field_count {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::FIELD_MISMATCH,
                    format!(
                        "`{type_text}->{name_text}` has {field_count} field{}, but the pattern has {}",
                        if field_count == 1 { "" } else { "s" },
                        sub_patterns.len()
                    ),
                    span,
                )
                .with_help("give one pattern per field, in order; `_` matches any value"),
            );
            return Pat::Invalid(sub_patterns);
        }
        Pat::Variant {
            ty,
            variant,
            fields: sub_patterns,
            type_args,
        }
    }

    /// `:local` or `:const` inside a function.
    fn lower_let(
        &mut self,
        kind: LocalKind,
        name: Spanned<String>,
        ty: Option<ast::Type>,
        value: Option<ast::Expr>,
    ) -> Stmt {
        // The value is lowered first: it cannot refer to the variable being declared.
        let init = value.map(|v| self.lower_expr(&v));
        let ty = {
            let scope = self.type_scope();
            ty.map(|t| lower_type(&t, &scope, self.diagnostics))
        };
        if kind == LocalKind::Const && init.is_none() {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::INCOMPLETE_DECLARATION,
                    format!("constant `{}` has no value", name.value),
                    name.span,
                )
                .with_help("use `:local` for a variable that is assigned later"),
            );
        }
        let local = self.declare(name, kind, ty);
        Stmt::Let { local, init }
    }

    fn lower_optional_expr(&mut self, expr: Option<ast::Expr>, fallback: Span) -> ExprId {
        match expr {
            Some(expr) => self.lower_expr(&expr),
            None => self.alloc_expr(Expr::Missing, fallback),
        }
    }

    fn lower_loop_body(&mut self, block: Option<ast::Block>) -> Block {
        self.loop_depth += 1;
        let body = self.lower_optional_block(block);
        self.loop_depth -= 1;
        body
    }

    fn lower_if(&mut self, if_stmt: &ast::IfStmt) -> StmtId {
        self.check_duplicate_args(if_stmt.syntax());
        let span = if_stmt.span();
        let cond = self.lower_optional_expr(if_stmt.condition(), span);
        let then_block = self.lower_optional_block(if_stmt.then_block());
        let else_branch = if_stmt.else_arg().and_then(|arg| {
            if let Some(nested) = arg.if_stmt() {
                Some(ElseBranch::If(self.lower_if(&nested)))
            } else {
                arg.block().map(|b| ElseBranch::Block(self.lower_block(&b)))
            }
        });
        self.alloc_stmt(
            Stmt::If {
                cond,
                then_block,
                else_branch,
            },
            span,
        )
    }

    fn lower_for(&mut self, for_stmt: &ast::ForStmt) -> Stmt {
        self.check_duplicate_args(for_stmt.syntax());
        let span = for_stmt.span();
        let arg_expr = |ctx: &mut Self, name: &str| {
            for_stmt
                .arg(name)
                .and_then(|a| a.expr())
                .map(|e| (ctx.lower_expr(&e), e.span()))
        };
        let from = arg_expr(self, "from")
            .map_or_else(|| self.alloc_expr(Expr::Missing, span), |(id, _)| id);
        let to = arg_expr(self, "to");
        let until = arg_expr(self, "until");
        let step = arg_expr(self, "step");

        let end = match (to, until) {
            (Some((to, _)), None) => Some((ForEnd::To, to)),
            (None, Some((until, _))) => Some((ForEnd::Until, until)),
            (Some(_), Some((_, until_span))) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::INVALID_FORM_ARGUMENTS,
                        "`:for` takes either `to=` or `until=`, not both",
                        until_span,
                    )
                    .with_help("`to=` includes the bound and `until=` excludes it"),
                );
                None
            }
            (None, None) => {
                self.diagnostics.push(Diagnostic::error(
                    codes::INVALID_FORM_ARGUMENTS,
                    "`:for` needs a bound: `to=` (inclusive) or `until=` (exclusive)",
                    for_stmt.keyword_span(),
                ));
                None
            }
        };
        if let Some((step, step_span)) = step
            && matches!(
                self.body.exprs[step],
                Expr::Literal(Literal::Int { value: 0, .. })
            )
        {
            self.diagnostics.push(Diagnostic::error(
                codes::INVALID_FORM_ARGUMENTS,
                "the step of a `:for` loop cannot be zero",
                step_span,
            ));
        }

        self.push_scope();
        let var = self.declare(
            spanned_name(for_stmt.var(), for_stmt.keyword_span()),
            LocalKind::LoopVar,
            None,
        );
        let body = self.lower_loop_body(for_stmt.body());
        self.pop_scope();
        Stmt::For {
            var,
            from,
            end,
            step: step.map(|(id, _)| id),
            body,
        }
    }

    fn lower_set_target(&mut self, set: &ast::SetStmt) -> Place {
        let target = match set.target() {
            Some(ast::SetTarget::Name(name)) => Some((name.text(), name.span())),
            Some(ast::SetTarget::Expr(ast::Expr::ParenExpr(paren))) => match paren.expr() {
                Some(ast::Expr::VarExpr(var)) => Some((var.name(), var.span())),
                Some(ast::Expr::MemberExpr(member)) => return self.lower_field_target(&member),
                other => {
                    let span = other.map_or_else(|| paren.span(), |e| e.span());
                    self.diagnostics.push(Diagnostic::error(
                        codes::INVALID_ASSIGNMENT_TARGET,
                        "only variables can be assigned with `:set`",
                        span,
                    ));
                    return Place::Error;
                }
            },
            // `:set $x ...` was reported by the parser; resolve it anyway to avoid more errors.
            Some(ast::SetTarget::Expr(ast::Expr::VarExpr(var))) => Some((var.name(), var.span())),
            Some(ast::SetTarget::Expr(other)) => {
                self.diagnostics.push(Diagnostic::error(
                    codes::INVALID_ASSIGNMENT_TARGET,
                    "only variables can be assigned with `:set`",
                    other.span(),
                ));
                return Place::Error;
            }
            None => None,
        };
        let Some((name, span)) = target else {
            return Place::Error;
        };
        self.assignable_variable(&name, span, false)
    }

    /// The target of `:set ($p->field) ...`: a field of a variable that may be modified.
    fn lower_field_target(&mut self, member: &ast::MemberExpr) -> Place {
        // Find the variable at the root of the chain of fields.
        let mut root = member.clone();
        let variable = loop {
            match root.base() {
                Some(ast::Expr::MemberExpr(inner)) => root = inner,
                Some(ast::Expr::VarExpr(var)) => break var,
                other => {
                    let span = other.map_or_else(|| member.span(), |e| e.span());
                    self.diagnostics.push(Diagnostic::error(
                        codes::INVALID_ASSIGNMENT_TARGET,
                        "only fields of variables can be assigned with `:set`",
                        span,
                    ));
                    return Place::Error;
                }
            }
        };
        if self.assignable_variable(&variable.name(), variable.span(), true) == Place::Error {
            return Place::Error;
        }
        let field = self.lower_expr(&ast::Expr::MemberExpr(member.clone()));
        Place::Part(field)
    }

    /// The variable `name` as the target of `:set`, or the root of the field assigned by it
    /// (`of_field`), reporting variables that cannot be modified.
    fn assignable_variable(&mut self, name: &str, span: Span, of_field: bool) -> Place {
        let name = name.to_owned();
        if let Some(local) = self.resolve_local(&name, span) {
            let declaration = &self.body.locals[local];
            if !declaration.kind.is_mutable() {
                let help = match declaration.kind {
                    LocalKind::Const => {
                        format!("declare it with `:local {name}` to allow reassignment")
                    }
                    LocalKind::Param(_) => {
                        format!("declare the parameter as `mut {name}` to modify it")
                    }
                    LocalKind::LoopVar => "loop variables cannot be reassigned".to_owned(),
                    LocalKind::Binding => {
                        "names bound by a pattern cannot be reassigned".to_owned()
                    }
                    LocalKind::Element { .. } => {
                        format!("write `:foreach mut {name} ...` to modify the elements")
                    }
                    LocalKind::Caught => {
                        format!("copy it first: `:local mine [${name}->clone]`")
                    }
                    LocalKind::Captured => format!(
                        "a closure captures a copy of `{name}`, which it can only read; compute a new value instead"
                    ),
                    LocalKind::Var => unreachable!("`:local` variables are mutable"),
                };
                let declared_at = declaration.name.span;
                let message = if declaration.kind == LocalKind::Captured {
                    format!("cannot modify `{name}`: it is captured by this function value")
                } else if of_field {
                    format!("cannot assign to a field of `{name}`: it cannot be modified")
                } else {
                    format!("cannot assign twice to `{name}`")
                };
                self.diagnostics.push(
                    Diagnostic::error(codes::IMMUTABLE_ASSIGNMENT, message, span)
                        .with_secondary(declared_at, "declared here")
                        .with_help(help),
                );
            }
            return Place::Local(local);
        }
        match self.names.items.values.get(&name) {
            Some(ValueItem::Global(global)) => Place::Global(*global),
            Some(ValueItem::Const(_)) => {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::IMMUTABLE_ASSIGNMENT,
                        format!("cannot assign to the constant `{name}`"),
                        span,
                    )
                    .with_help("use `:global` for a module-level variable that changes"),
                );
                Place::Error
            }
            None => {
                self.report_unresolved(&name, span);
                Place::Error
            }
        }
    }

    // ----- Expressions ---------------------------------------------------------------------

    fn lower_expr(&mut self, expr: &ast::Expr) -> ExprId {
        let span = expr.span();
        let lowered = match expr {
            ast::Expr::Literal(lit) => lower_literal(lit),
            ast::Expr::StringLit(string) => self.lower_string(string),
            ast::Expr::VarExpr(var) => self.resolve_var(&var.name(), span),
            ast::Expr::ParenExpr(paren) => {
                // Parentheses only group: reuse the inner expression.
                return self.lower_optional_expr(paren.expr(), span);
            }
            ast::Expr::BracketExpr(bracket) => {
                match bracket.command() {
                    Some(ast::Stmt::Call(call)) => return self.lower_call(&call),
                    Some(ast::Stmt::FnDecl(decl)) => {
                        if self.in_function {
                            self.lower_closure(&decl)
                        } else {
                            self.diagnostics.push(Diagnostic::error(
                                codes::NOT_CONST_EVALUABLE,
                                "module-level initializers must be constant, and cannot create functions",
                                decl.keyword_span(),
                            ));
                            Expr::Missing
                        }
                    }
                    Some(other) => {
                        self.diagnostics.push(
                        Diagnostic::error(
                            codes::NOT_SUPPORTED_YET,
                            "only calls can be used inside `[...]`",
                            other.span(),
                        )
                        .with_help("expression-valued `:if` and `:match` are reserved for a later version"),
                    );
                        Expr::Missing
                    }
                    None => Expr::Missing,
                }
            }
            ast::Expr::PrefixExpr(prefix) => {
                let operand = self.lower_optional_expr(prefix.operand(), span);
                match prefix.op() {
                    Some(op) => Expr::Unary { op, operand },
                    None => Expr::Missing,
                }
            }
            ast::Expr::BinExpr(bin) => {
                let lhs = self.lower_optional_expr(bin.lhs(), span);
                let rhs = self.lower_optional_expr(bin.rhs(), span);
                match (bin.op(), bin.op_span()) {
                    (Some(op), Some(op_span)) => Expr::Binary {
                        op,
                        op_span,
                        lhs,
                        rhs,
                    },
                    _ => Expr::Missing,
                }
            }
            ast::Expr::CastExpr(cast) => {
                let inner = self.lower_optional_expr(cast.expr(), span);
                match cast.ty() {
                    Some(ty) => Expr::Cast {
                        expr: inner,
                        ty: {
                            let scope = self.type_scope();
                            lower_type(&ty, &scope, self.diagnostics)
                        },
                    },
                    None => Expr::Missing,
                }
            }
            ast::Expr::PathExpr(path) => self.lower_path_value(path),
            ast::Expr::BraceLit(brace) => self.lower_brace_lit(brace),
            ast::Expr::MemberExpr(member) => self.lower_member(member),
            // A type name in value position was reported by the parser.
            ast::Expr::TypeName(_) => Expr::Missing,
        };
        self.alloc_expr(lowered, span)
    }

    /// `/module/name` in value position: a constant or global of a module, or a function.
    fn lower_path_value(&mut self, path: &ast::PathExpr) -> Expr {
        let Some((path_type, module_path)) = path_parts(path) else {
            return Expr::Missing;
        };
        if let Some(list) = path_type.generic_args() {
            self.diagnostics.push(Diagnostic::error(
                codes::INVALID_FORM_ARGUMENTS,
                "type arguments are given to calls of functions, not to values",
                list.span(),
            ));
        }
        match self.names.path_value(&module_path) {
            Ok(PathValue::Item(ValueItem::Const(id))) => Expr::Const(id),
            Ok(PathValue::Item(ValueItem::Global(id))) => Expr::Global(id),
            Ok(PathValue::Fn(id)) if self.in_function => Expr::FnRef(id),
            Ok(PathValue::Fn(_)) => {
                self.diagnostics.push(Diagnostic::error(
                    codes::NOT_CONST_EVALUABLE,
                    "module-level initializers must be constant, and cannot use functions as values",
                    path.span(),
                ));
                Expr::Missing
            }
            Err(diagnostic) => {
                self.diagnostics.push(*diagnostic);
                Expr::Missing
            }
        }
    }

    /// The type a path names, when it is the base of `->`, as `/geo/Color` in
    /// `/geo/Color->red`; `None` when it names a value.
    fn path_as_type(&self, base: &ast::Expr) -> Option<ast::PathType> {
        let ast::Expr::PathExpr(path) = base else {
            return None;
        };
        let (path_type, module_path) = path_parts(path)?;
        self.names.path_is_type(&module_path).then_some(path_type)
    }

    fn lower_string(&mut self, string: &ast::StringLit) -> Expr {
        let mut parts = Vec::new();
        for part in string.parts() {
            match part {
                ast::StringPart::Text(token) => {
                    let (text, _) = literal::unescape(token.text());
                    parts.push(StringPart::Text(text));
                }
                ast::StringPart::Var(token) => {
                    let span = ast::span_of(token.text_range());
                    let expr = self.resolve_var(&token.text()[1..], span);
                    parts.push(StringPart::Expr(self.alloc_expr(expr, span)));
                }
                ast::StringPart::Interpolation(interpolation) => {
                    let span = interpolation.span();
                    let id = if let Some(expr) = interpolation.expr() {
                        self.lower_expr(&expr)
                    } else if let Some(ast::Stmt::Call(call)) = interpolation.command() {
                        self.lower_call(&call)
                    } else {
                        self.alloc_expr(Expr::Missing, span)
                    };
                    parts.push(StringPart::Expr(id));
                }
            }
        }
        Expr::String(parts)
    }

    fn resolve_var(&mut self, name: &str, span: Span) -> Expr {
        if let Some(local) = self.resolve_local(name, span) {
            return Expr::Local(local);
        }
        match self.names.items.values.get(name) {
            Some(ValueItem::Const(id)) => Expr::Const(*id),
            Some(ValueItem::Global(id)) => Expr::Global(*id),
            None if self.in_function && self.names.items.fns.contains_key(name) => {
                Expr::FnRef(self.names.items.fns[name])
            }
            None => {
                self.report_unresolved(name, span);
                Expr::Missing
            }
        }
    }

    fn report_unresolved(&mut self, name: &str, span: Span) {
        let mut diagnostic = Diagnostic::error(
            codes::UNRESOLVED_VARIABLE,
            format!("cannot find variable `{name}` in this scope"),
            span,
        );
        if !self.in_function {
            diagnostic = diagnostic.with_help(
                "module-level initializers are evaluated at compile time and can only use \
                 constants; for a value computed when the program runs, use `:local`",
            );
        } else if self.names.script_locals.contains(name) {
            diagnostic = diagnostic.with_help(
                "top-level `:local` variables belong to the script, not to functions; use `:const` \
                 or `:global` to share a value with functions",
            );
        } else if self.names.items.fns.contains_key(name) {
            diagnostic = diagnostic.with_help(format!(
                "`{name}` is a function; call it as a command: `[:{name} ...]`"
            ));
        }
        self.diagnostics.push(diagnostic);
    }

    /// A call, with its head recorded for the check of its `?`.
    fn lower_call(&mut self, call: &ast::Call) -> ExprId {
        let id = self.lower_call_unrecorded(call);
        if let Some((text, span)) = call.head_text() {
            let mark = call.raise_mark().map(|t| ast::span_of(t.text_range()));
            self.body
                .call_heads
                .insert(id, CallHead { text, span, mark });
        }
        id
    }

    fn lower_call_unrecorded(&mut self, call: &ast::Call) -> ExprId {
        let span = call.span();
        if call.has_error_head() {
            return self.alloc_expr(Expr::Missing, span);
        }
        let mut fn_args: TypeArgs = None;
        let callee = match call.callee() {
            Some(ast::Callee::Command(command)) => {
                let Some(name) = command.name() else {
                    return self.alloc_expr(Expr::Missing, span);
                };
                fn_args = command
                    .generic_args()
                    .map(|list| self.lower_type_args(&list));
                let name = name.text().to_owned();
                match name.as_str() {
                    "return" | "break" | "continue" => return self.lower_control(&name, call),
                    "error" => return self.lower_raise(call),
                    _ => {
                        if let Some(builtin) = Builtin::from_name(&name) {
                            Callee::Builtin(builtin)
                        } else if let Some(&id) = self.names.items.fns.get(&name) {
                            Callee::Fn(id)
                        } else {
                            let mut diagnostic = Diagnostic::error(
                                codes::UNKNOWN_COMMAND,
                                format!("unknown command `:{name}`"),
                                command.span(),
                            );
                            if self.lookup_local(&name).is_some()
                                || self.names.items.values.contains_key(&name)
                            {
                                diagnostic = diagnostic
                                    .with_help(format!("to read the variable, write `${name}`"));
                            }
                            self.diagnostics.push(diagnostic);
                            Callee::Error
                        }
                    }
                }
            }
            Some(ast::Callee::Some(_)) => {
                let value = self.single_argument(call, "some", "`[some $value]`");
                return self.alloc_expr(Expr::Some(value), span);
            }
            Some(ast::Callee::Expr(ast::Expr::MemberExpr(member))) => {
                if let Some(call) = self.lower_method_call(call, &member) {
                    return call;
                }
                Callee::Error
            }
            Some(ast::Callee::Expr(ast::Expr::PathExpr(path))) => {
                let Some((path_type, module_path)) = path_parts(&path) else {
                    return self.alloc_expr(Expr::Missing, span);
                };
                fn_args = path_type
                    .generic_args()
                    .map(|list| self.lower_type_args(&list));
                match self.names.path_fn(&module_path) {
                    Ok(id) => Callee::Fn(id),
                    Err(diagnostic) => {
                        self.diagnostics.push(*diagnostic);
                        Callee::Error
                    }
                }
            }
            Some(ast::Callee::Expr(
                expr @ (ast::Expr::Literal(_) | ast::Expr::StringLit(_) | ast::Expr::BraceLit(_)),
            )) => self.literal_callee(&expr),
            Some(ast::Callee::Expr(expr)) => return self.lower_value_call(call, &expr),
            None => Callee::Error,
        };

        if let (Some(list), Callee::Builtin(builtin)) = (&fn_args, callee)
            && builtin != Builtin::Default
        {
            self.diagnostics.push(Diagnostic::error(
                codes::INVALID_FORM_ARGUMENTS,
                format!("`:{}` takes no type arguments", builtin.name()),
                list.span,
            ));
        }
        let mut args = self.lower_call_args(call);
        if callee == Callee::Builtin(Builtin::Assert) {
            self.default_assert_message(call, &mut args);
        }
        self.alloc_expr(
            Expr::Call {
                callee,
                args,
                owner_args: None,
                fn_args,
                self_ty: None,
            },
            span,
        )
    }

    /// The message of an `:assert` written without one: its condition, as written.
    fn default_assert_message(&mut self, call: &ast::Call, args: &mut Vec<CallArg>) {
        let written = call.args();
        let [ast::Arg::Positional(condition)] = written.as_slice() else {
            return;
        };
        let text = condition.syntax().text().to_string();
        let value = self.alloc_expr(Expr::String(vec![StringPart::Text(text)]), condition.span());
        args.push(CallArg { name: None, value });
    }

    /// Reports a literal as the head of a command, which only a method call can have.
    fn literal_callee(&mut self, literal: &ast::Expr) -> Callee {
        self.diagnostics.push(
            Diagnostic::error(
                codes::UNKNOWN_COMMAND,
                "a literal cannot be called",
                literal.span(),
            )
            .with_help("to call a method of the value, write `[value->method ...]`"),
        );
        Callee::Error
    }

    /// `[$f args]`: a call of a function value, with positional arguments.
    fn lower_value_call(&mut self, call: &ast::Call, callee: &ast::Expr) -> ExprId {
        let callee = self.lower_expr(callee);
        let mut args = Vec::new();
        for arg in self.lower_call_args(call) {
            if let Some(name) = &arg.name {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::INVALID_FORM_ARGUMENTS,
                        "the arguments of a function value are positional",
                        name.span,
                    )
                    .with_help("a function value has no parameter names; pass the values in order"),
                );
            }
            args.push(arg.value);
        }
        self.alloc_expr(Expr::CallValue { callee, args }, call.span())
    }

    /// Written type arguments, as in `:max<f64>`.
    fn lower_type_args(&mut self, list: &ast::GenericArgList) -> Spanned<Vec<Ty>> {
        let scope = self.type_scope();
        let args = list
            .types()
            .iter()
            .map(|t| lower_type(t, &scope, self.diagnostics).value)
            .collect();
        Spanned {
            value: args,
            span: list.span(),
        }
    }

    /// Reports type arguments written after a member that is not a function, such as a field
    /// or a variant.
    fn reject_member_type_args(&mut self, member: &ast::MemberExpr, what: &str, help: &str) {
        if let Some(list) = member.generic_args() {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::INVALID_FORM_ARGUMENTS,
                    format!("{what} takes no type arguments"),
                    list.span(),
                )
                .with_help(help.to_owned()),
            );
        }
    }

    /// The single positional argument of `[some value]` or `[Box->new value]`; other
    /// arguments are reported.
    fn single_argument(&mut self, call: &ast::Call, what: &str, example: &str) -> ExprId {
        let args = self.lower_call_args(call);
        if let [CallArg { name: None, value }] = args.as_slice() {
            return *value;
        }
        self.diagnostics.push(
            Diagnostic::error(
                codes::INVALID_FORM_ARGUMENTS,
                format!("`{what}` takes exactly one value"),
                call.span(),
            )
            .with_help(format!("write {example}")),
        );
        self.alloc_expr(Expr::Missing, call.span())
    }

    /// `[$receiver->method args]`. Returns `None` (after reporting) for forms that are not
    /// method calls on a value, such as `Type->member`.
    fn lower_method_call(&mut self, call: &ast::Call, member: &ast::MemberExpr) -> Option<ExprId> {
        let span = call.span();
        let base = member.base()?;
        if let ast::Expr::TypeName(type_name) = &base {
            return self.lower_associated_call(call, member, type_name);
        }
        if let Some(type_name) = self.path_as_type(&base) {
            return self.lower_associated_call(call, member, &type_name);
        }
        let Some(ast::Member::Name(name)) = member.member() else {
            self.diagnostics.push(Diagnostic::error(
                codes::INVALID_FORM_ARGUMENTS,
                "only methods can be called; an index or key cannot",
                member.span(),
            ));
            return None;
        };
        let receiver = self.lower_expr(&base);
        let method = Spanned {
            value: name.text(),
            span: name.span(),
        };
        let type_args = member
            .generic_args()
            .map(|list| self.lower_type_args(&list));
        let args = self.lower_call_args(call);
        Some(self.alloc_expr(
            Expr::MethodCall {
                receiver,
                method,
                args,
                type_args,
            },
            span,
        ))
    }

    /// The user-defined type a type name in an expression refers to, such as the `Point` of
    /// `[Point->new 1 2]`. Reports names that are not user-defined types.
    fn type_named(&mut self, type_name: &ast::PathType) -> Option<(TypeId, TypeArgs)> {
        let span = type_name.span();
        let Some(name) = type_name.name() else {
            let module_path = type_name.path()?;
            return match self.names.path_type(&module_path) {
                Ok((id, ty)) => {
                    let written = module_path.syntax().text().to_string();
                    self.with_type_args(type_name, &written, id, ty)
                }
                Err(diagnostic) => {
                    self.diagnostics.push(*diagnostic);
                    None
                }
            };
        };
        let name = name.text();
        let found = if name == "Self" {
            self.self_ty.and_then(|ty| {
                self.names
                    .items
                    .types
                    .values()
                    .find(|(_, t)| *t == ty)
                    .map(|&(id, _)| id)
            })
        } else {
            self.names.items.types.get(&name).map(|&(id, _)| id)
        };
        if found.is_none() {
            let message = if name == "Self" {
                "`Self` can only be used inside a struct or an enum".to_owned()
            } else if Ty::from_name(&name).is_some() {
                format!("the built-in type `{name}` has no associated functions")
            } else {
                format!("unknown type `{name}`")
            };
            self.diagnostics
                .push(Diagnostic::error(codes::UNKNOWN_TYPE, message, span));
        }
        let id = found?;
        let ty = self.names.shared.type_ty(id);
        // `Self` stands for the type with its own parameters.
        if name == "Self" {
            let arity = match ty {
                Ty::Adt(adt) => adt.args.len(),
                _ => 0,
            };
            let args = self.self_ty.map(Ty::components).unwrap_or_default();
            return Some((id, (arity > 0).then_some(Spanned { value: args, span })));
        }
        self.with_type_args(type_name, &name, id, ty)
    }

    /// The declared type `id`, whose type is `ty`, written `name` with the type arguments
    /// written in `type_name`.
    fn with_type_args(
        &mut self,
        type_name: &ast::PathType,
        name: &str,
        id: TypeId,
        ty: Ty,
    ) -> Option<(TypeId, TypeArgs)> {
        let arity = match ty {
            Ty::Adt(adt) => adt.args.len(),
            _ => 0,
        };
        let Some(list) = type_name.generic_args() else {
            return Some((id, None));
        };
        let args: Vec<Ty> = {
            let scope = self.type_scope();
            list.types()
                .iter()
                .map(|t| lower_type(t, &scope, self.diagnostics).value)
                .collect()
        };
        if let Some(diagnostic) = type_args_error(name, arity, args.len(), list.span()) {
            self.diagnostics.push(diagnostic);
            return None;
        }
        Some((
            id,
            Some(Spanned {
                value: args,
                span: list.span(),
            }),
        ))
    }

    /// `[Type->function args]`: a call of an associated function or method of a type, the
    /// construction of an enum variant, or `[Box->new value]`.
    fn lower_associated_call(
        &mut self,
        call: &ast::Call,
        member: &ast::MemberExpr,
        type_name: &ast::PathType,
    ) -> Option<ExprId> {
        let Some(ast::Member::Name(name)) = member.member() else {
            self.diagnostics.push(Diagnostic::error(
                codes::INVALID_FORM_ARGUMENTS,
                "expected the name of a function after `->`",
                member.span(),
            ));
            return None;
        };
        let is_box = type_name.name().is_some_and(|n| n.text() == "Box")
            && !self.names.items.types.contains_key("Box");
        if is_box {
            self.reject_member_type_args(
                member,
                "`Box->new`",
                "the type of the box comes from its value",
            );
            if name.text() != "new" {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::FIELD_MISMATCH,
                        format!("`Box` has no function named `{}`", name.text()),
                        name.span(),
                    )
                    .with_help("create a box with `[Box->new $value]`"),
                );
                return None;
            }
            let value = self.single_argument(call, "Box->new", "`[Box->new $value]`");
            return Some(self.alloc_expr(Expr::BoxNew(value), call.span()));
        }
        if let Some((param_name, param)) = self.param_named(type_name) {
            let traits = self
                .trait_bounds
                .get(&param_name)
                .cloned()
                .unwrap_or_default();
            let function = self.trait_function(&traits, &name, &param_name)?;
            return Some(self.lower_trait_call(call, member, function, None, Some(param)));
        }
        if let Some(head) = type_name.name().and_then(|n| TypeHead::named(&n.text())) {
            return self.lower_impl_call(call, member, type_name, head, &name);
        }
        let (ty, type_args) = self.type_named(type_name)?;
        if let Some(variant) = self.variant_index(ty, &name.text()) {
            self.reject_member_type_args(
                member,
                "a variant",
                "write the enum's type arguments before `->`, as in `Opt<i64>->some`",
            );
            let args = self.lower_call_args(call);
            return Some(self.alloc_expr(
                Expr::Variant {
                    ty,
                    variant,
                    args,
                    type_args,
                },
                call.span(),
            ));
        }
        let function = self
            .names
            .shared
            .members
            .get(&ty)
            .and_then(|members| members.functions.get(&name.text()).copied());
        let Some(function) = function else {
            // A function the type gets from a trait it implements.
            let traits = self.names.shared.trait_closure(
                self.names
                    .shared
                    .type_impls
                    .get(&ty)
                    .cloned()
                    .unwrap_or_default(),
            );
            let type_text = type_name.syntax().text().to_string();
            let function = self.trait_function(&traits, &name, &type_text)?;
            let self_ty = self.names.shared.type_ty(ty);
            return Some(self.lower_trait_call(call, member, function, type_args, Some(self_ty)));
        };
        let fn_args = member
            .generic_args()
            .map(|list| self.lower_type_args(&list));
        let args = self.lower_call_args(call);
        Some(self.alloc_expr(
            Expr::Call {
                callee: Callee::Fn(function),
                args,
                owner_args: type_args,
                fn_args,
                self_ty: None,
            },
            call.span(),
        ))
    }

    /// The type parameter a type name in an expression refers to, with its name, as the `T`
    /// of `[T->make]`.
    fn param_named(&mut self, type_name: &ast::PathType) -> Option<(String, Ty)> {
        let name = type_name.name()?.text();
        let &(_, ty) = self.params.iter().find(|(n, _)| *n == name)?;
        if let Some(list) = type_name.generic_args() {
            self.diagnostics.push(Diagnostic::error(
                codes::UNKNOWN_TYPE,
                format!("the type parameter `{name}` does not take type arguments"),
                list.span(),
            ));
        }
        Some((name, ty))
    }

    /// The function `name` of one of `traits`, called on `on`; reports none or several.
    fn trait_function(
        &mut self,
        traits: &[TraitId],
        name: &ast::NameRef,
        on: &str,
    ) -> Option<FnId> {
        let found = self.names.shared.trait_functions(traits, &name.text());
        match found.as_slice() {
            [function] => Some(*function),
            [] => {
                self.diagnostics.push(Diagnostic::error(
                    codes::FIELD_MISMATCH,
                    format!("`{on}` has no function named `{}`", name.text()),
                    name.span(),
                ));
                None
            }
            _ => {
                self.diagnostics.push(Diagnostic::error(
                    codes::FIELD_MISMATCH,
                    format!(
                        "`{on}` has several functions named `{}`, from different traits",
                        name.text()
                    ),
                    name.span(),
                ));
                None
            }
        }
    }

    /// A call of function `function` of a trait or an `:impl` on type `self_ty`, as in
    /// `[T->make]` or `[String->from_chars $chars]`; without `self_ty`, the type is inferred.
    fn lower_trait_call(
        &mut self,
        call: &ast::Call,
        member: &ast::MemberExpr,
        function: FnId,
        owner_args: TypeArgs,
        self_ty: Option<Ty>,
    ) -> ExprId {
        let fn_args = member
            .generic_args()
            .map(|list| self.lower_type_args(&list));
        let args = self.lower_call_args(call);
        self.alloc_expr(
            Expr::Call {
                callee: Callee::Fn(function),
                args,
                owner_args,
                fn_args,
                self_ty,
            },
            call.span(),
        )
    }

    /// `[Type->function args]` for a built-in type: a function that an `:impl` gives it. The
    /// type is the one written, or inferred when type arguments it needs are left out, as in
    /// `[List->repeat 0 5]`.
    fn lower_impl_call(
        &mut self,
        call: &ast::Call,
        member: &ast::MemberExpr,
        type_name: &ast::PathType,
        head: TypeHead,
        name: &ast::NameRef,
    ) -> Option<ExprId> {
        let found = self
            .names
            .shared
            .impl_fns
            .get(&(head, name.text()))
            .copied();
        let Some(function) = found else {
            self.diagnostics.push(Diagnostic::error(
                codes::FIELD_MISMATCH,
                format!(
                    "`{}` has no function named `{}`",
                    type_name.syntax().text(),
                    name.text()
                ),
                name.span(),
            ));
            return None;
        };
        let complete = matches!(head, TypeHead::Scalar(_)) || type_name.generic_args().is_some();
        let self_ty = complete.then(|| {
            let scope = self.type_scope();
            lower_type(
                &ast::Type::Path(type_name.clone()),
                &scope,
                self.diagnostics,
            )
            .value
        });
        Some(self.lower_trait_call(call, member, function, None, self_ty))
    }

    /// The index of the variant `name` of `ty`, if `ty` is an enum with such a variant.
    fn variant_index(&self, ty: TypeId, name: &str) -> Option<usize> {
        self.names
            .shared
            .members
            .get(&ty)?
            .variants
            .iter()
            .position(|(v, _)| v == name)
    }

    /// A struct literal: `Point{x=1.0; y=2.0}`.
    fn lower_brace_lit(&mut self, brace: &ast::BraceLit) -> Expr {
        let span = brace.span();
        let Some(type_name) = brace.type_prefix() else {
            return self.lower_collection(brace, None);
        };
        let type_text = type_name.name().map(|n| n.text()).unwrap_or_default();
        if matches!(type_text.as_str(), "List" | "Map" | "Set")
            && !self.names.items.types.contains_key(&type_text)
        {
            let declared = {
                let scope = self.type_scope();
                lower_type(&ast::Type::Path(type_name), &scope, self.diagnostics)
            };
            return self.lower_collection(brace, Some(declared));
        }
        let Some((strukt, type_args)) = self.type_named(&type_name) else {
            return Expr::Missing;
        };
        if self
            .names
            .shared
            .members
            .get(&strukt)
            .is_some_and(|m| !m.variants.is_empty())
        {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::FIELD_MISMATCH,
                    format!("`{type_text}` is an enum, not a struct"),
                    span,
                )
                .with_help(format!(
                    "build a value of a variant: `{type_text}->name` or `[{type_text}->name ...]`"
                )),
            );
            return Expr::Missing;
        }
        let declared = self
            .names
            .shared
            .members
            .get(&strukt)
            .map(|m| m.fields.clone())
            .unwrap_or_default();
        let values = self.lower_field_values(brace, &type_text, &declared);
        self.report_missing_fields(
            &declared,
            &values,
            &type_text,
            brace.open_brace_span().unwrap_or(span),
        );
        Expr::StructLit {
            strukt,
            fields: values,
            type_args,
        }
    }

    /// The values of the fields of a struct literal, by field index.
    fn lower_field_values(
        &mut self,
        brace: &ast::BraceLit,
        type_text: &str,
        declared: &[(String, bool)],
    ) -> Vec<Option<ExprId>> {
        let mut values: Vec<Option<ExprId>> = vec![None; declared.len()];
        let mut given: Vec<Option<Span>> = vec![None; declared.len()];
        for element in brace.elements() {
            let ast::BraceElement::Field(init) = element else {
                let element_span = match &element {
                    ast::BraceElement::Entry(entry) => entry.span(),
                    ast::BraceElement::Value(value) => value.span(),
                    ast::BraceElement::Field(_) => unreachable!("handled above"),
                };
                self.diagnostics.push(Diagnostic::error(
                    codes::FIELD_MISMATCH,
                    format!("a `{type_text}` literal lists its fields as `name=value`"),
                    element_span,
                ));
                continue;
            };
            let Some(name) = init.name() else { continue };
            let field_name = name.text();
            let Some(index) = declared.iter().position(|(n, _)| *n == field_name) else {
                let names: Vec<String> = declared.iter().map(|(n, _)| format!("`{n}`")).collect();
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::FIELD_MISMATCH,
                        format!("`{type_text}` has no field named `{field_name}`"),
                        name.span(),
                    )
                    .with_help(format!(
                        "the fields of `{type_text}` are {}",
                        names.join(", ")
                    )),
                );
                if let Some(value) = init.value() {
                    self.lower_expr(&value);
                }
                continue;
            };
            if let Some(previous) = given[index] {
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::DUPLICATE_ARGUMENT,
                        format!("field `{field_name}` is given more than once"),
                        name.span(),
                    )
                    .with_secondary(previous, "first given here"),
                );
            }
            given[index] = Some(name.span());
            values[index] = Some(self.lower_optional_expr(init.value(), init.span()));
        }
        values
    }

    /// A collection literal, with the type written before it.
    fn lower_collection(&mut self, brace: &ast::BraceLit, declared: Option<Spanned<Ty>>) -> Expr {
        let mut elements = Vec::new();
        for element in brace.elements() {
            match element {
                ast::BraceElement::Value(value) => {
                    elements.push(Element::Value(self.lower_expr(&value)));
                }
                ast::BraceElement::Entry(entry) => {
                    let key = self.lower_optional_expr(entry.key(), entry.span());
                    let value = self.lower_optional_expr(entry.value(), entry.span());
                    elements.push(Element::Entry(key, value));
                }
                ast::BraceElement::Field(init) => {
                    let name = init.name().map(|n| n.text()).unwrap_or_default();
                    self.diagnostics.push(
                        Diagnostic::error(
                            codes::FIELD_MISMATCH,
                            "the keys of a map literal are values",
                            init.span(),
                        )
                        .with_help(format!(
                            "write a string key in quotes: `\"{name}\"=...`, or a variable: `${name}=...`"
                        )),
                    );
                    if let Some(value) = init.value() {
                        self.lower_expr(&value);
                    }
                }
            }
        }
        Expr::Collection { declared, elements }
    }

    /// Reports the fields without a value or a default in a struct literal.
    fn report_missing_fields(
        &mut self,
        declared: &[(String, bool)],
        values: &[Option<ExprId>],
        type_text: &str,
        span: Span,
    ) {
        let missing: Vec<String> = declared
            .iter()
            .zip(values)
            .filter(|((_, has_default), value)| value.is_none() && !has_default)
            .map(|((name, _), _)| format!("`{name}`"))
            .collect();
        if !missing.is_empty() {
            self.diagnostics.push(Diagnostic::error(
                codes::FIELD_MISMATCH,
                format!(
                    "missing field{} {} in `{type_text}` literal",
                    if missing.len() == 1 { "" } else { "s" },
                    missing.join(", ")
                ),
                span,
            ));
        }
    }

    /// A member access in value position: a field read, `$p->x`, or an element, `$xs->0`.
    fn lower_member(&mut self, member: &ast::MemberExpr) -> Expr {
        let Some(base) = member.base() else {
            return Expr::Missing;
        };
        self.reject_member_type_args(
            member,
            "a member that is not called",
            "type arguments are given to calls of functions and methods, as in `[$a->map<f64> ...]`",
        );
        if let ast::Expr::TypeName(type_name) = base {
            return self.lower_static_member(member, &type_name);
        }
        if let Some(type_name) = self.path_as_type(&base) {
            return self.lower_static_member(member, &type_name);
        }
        match member.member() {
            Some(ast::Member::Name(name)) => {
                let base = self.lower_expr(&base);
                Expr::Field {
                    base,
                    field: Spanned {
                        value: name.text(),
                        span: name.span(),
                    },
                }
            }
            Some(ast::Member::Index(index)) => {
                let base = self.lower_expr(&base);
                let index = self.lower_expr(&index);
                Expr::Index { base, index }
            }
            None => Expr::Missing,
        }
    }

    /// `Type->name` in value position: a variant without fields.
    fn lower_static_member(&mut self, member: &ast::MemberExpr, type_name: &ast::PathType) -> Expr {
        let span = member.span();
        let Some(ast::Member::Name(name)) = member.member() else {
            self.diagnostics.push(Diagnostic::error(
                codes::INVALID_FORM_ARGUMENTS,
                "expected the name of a variant after `->`",
                member.span(),
            ));
            return Expr::Missing;
        };
        let Some((ty, type_args)) = self.type_named(type_name) else {
            return Expr::Missing;
        };
        let name_text = name.text();
        let type_text = type_name.syntax().text().to_string();
        if let Some(variant) = self.variant_index(ty, &name_text) {
            let fields = &self.names.shared.members[&ty].variants[variant].1;
            if !fields.is_empty() {
                let list: Vec<String> = fields.iter().map(|f| format!("${f}")).collect();
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::FIELD_MISMATCH,
                        format!("the variant `{type_text}->{name_text}` has fields"),
                        span,
                    )
                    .with_help(format!(
                        "construct it with its fields: `[{type_text}->{name_text} {}]`",
                        list.join(" ")
                    )),
                );
            }
            return Expr::Variant {
                ty,
                variant,
                args: Vec::new(),
                type_args,
            };
        }
        let is_function = self
            .names
            .shared
            .members
            .get(&ty)
            .is_some_and(|m| m.functions.contains_key(&name_text));
        let mut diagnostic = Diagnostic::error(
            codes::FIELD_MISMATCH,
            format!("`{type_text}` has no variant named `{name_text}`"),
            name.span(),
        );
        if is_function {
            diagnostic = diagnostic.with_help(format!(
                "`{name_text}` is a function; call it: `[{type_text}->{name_text} ...]`"
            ));
        }
        self.diagnostics.push(diagnostic);
        Expr::Missing
    }

    fn lower_call_args(&mut self, call: &ast::Call) -> Vec<CallArg> {
        call.args()
            .into_iter()
            .map(|arg| match arg {
                ast::Arg::Positional(expr) => CallArg {
                    name: None,
                    value: self.lower_expr(&expr),
                },
                ast::Arg::Named(named) => {
                    let name = named.name().map(|n| Spanned {
                        value: n.text(),
                        span: n.span(),
                    });
                    let value = self.lower_optional_expr(named.expr(), named.span());
                    CallArg { name, value }
                }
            })
            .collect()
    }

    /// `:return value?`, `:break` and `:continue`.
    fn lower_control(&mut self, name: &str, call: &ast::Call) -> ExprId {
        let span = call.span();
        let args = call.args();
        let mut positional = Vec::new();
        for arg in &args {
            match arg {
                ast::Arg::Positional(expr) => positional.push(expr.clone()),
                ast::Arg::Named(named) => self.diagnostics.push(Diagnostic::error(
                    codes::INVALID_FORM_ARGUMENTS,
                    format!("`:{name}` does not take named arguments"),
                    named.span(),
                )),
            }
        }
        let allowed = usize::from(name == "return");
        if let Some(extra) = positional.get(allowed) {
            let message = if allowed == 0 {
                format!("`:{name}` does not take arguments")
            } else {
                "`:return` takes at most one value".to_owned()
            };
            self.diagnostics.push(Diagnostic::error(
                codes::INVALID_FORM_ARGUMENTS,
                message,
                extra.span(),
            ));
        }
        let expr = if name == "return" {
            Expr::Return(positional.first().map(|e| self.lower_expr(e)))
        } else {
            if self.loop_depth == 0 {
                self.diagnostics.push(Diagnostic::error(
                    codes::OUTSIDE_LOOP,
                    format!("`:{name}` can only be used inside a loop"),
                    span,
                ));
            }
            if name == "break" {
                Expr::Break
            } else {
                Expr::Continue
            }
        };
        self.alloc_expr(expr, span)
    }

    /// Reports parts of `expr` that cannot be evaluated at compile time.
    fn require_const(&mut self, expr: ExprId, what: &str) {
        let span = self.body.expr_spans[expr];
        let problem = match &self.body.exprs[expr] {
            Expr::Missing | Expr::Literal(_) | Expr::Const(_) | Expr::None => None,
            &Expr::Some(value) => {
                self.require_const(value, what);
                None
            }
            Expr::Variant { args, .. } => {
                for arg in args.clone() {
                    self.require_const(arg.value, what);
                }
                None
            }
            Expr::BoxNew(_) => Some("boxes, which are allocated when the program runs"),
            Expr::Raise { .. } => Some("`:error`"),
            Expr::Closure(_) | Expr::FnRef(_) => Some("function values"),
            // An empty collection allocates nothing.
            Expr::Collection { elements, .. } if elements.is_empty() => None,
            Expr::Collection { .. } => Some(
                "elements of collections, which are allocated when the program runs; start empty and add them in a function",
            ),
            Expr::Index { .. } => Some("indexing"),
            Expr::String(parts) => {
                for part in parts.clone() {
                    if let StringPart::Expr(part) = part {
                        self.require_const(part, what);
                    }
                }
                None
            }
            &Expr::Unary { operand, .. } => {
                self.require_const(operand, what);
                None
            }
            &Expr::Binary { lhs, rhs, .. } => {
                self.require_const(lhs, what);
                self.require_const(rhs, what);
                None
            }
            &Expr::Cast { expr: inner, .. } | &Expr::Field { base: inner, .. } => {
                self.require_const(inner, what);
                None
            }
            Expr::StructLit { fields, .. } => {
                for field in fields.clone().into_iter().flatten() {
                    self.require_const(field, what);
                }
                None
            }
            Expr::Local(_) => Some("variables"),
            Expr::Global(_) => Some("globals"),
            Expr::Call { .. } | Expr::MethodCall { .. } | Expr::CallValue { .. } => Some("calls"),
            Expr::Return(_) | Expr::Break | Expr::Continue => Some("control flow"),
        };
        if let Some(problem) = problem {
            self.diagnostics.push(
                Diagnostic::error(
                    codes::NOT_CONST_EVALUABLE,
                    format!("{what} must be constant, and cannot use {problem}"),
                    span,
                )
                .with_help(
                    "use literals, operators, casts and other constants; for a value computed \
                     when the program runs, use `:local`",
                ),
            );
        }
    }
}
