//! The type checker.

use la_arena::ArenaMap;
use pika_diagnostics::{Diagnostic, Span};
use pika_hir::{
    AdtKind, BinaryOp, Block, Body, Bound, Builtin, BuiltinTrait, CallArg, Callee, ConstId,
    Convention, Element, ElseBranch, Expr, ExprId, FloatTy, FnId, FnKind, FnOwner, Function,
    GenericParam, IntTy, Literal, LocalId, LocalKind, MatchArm, Module, ModuleId, Pat, PatId,
    Place, PreludeTrait, Spanned, Stmt, StmtId, StringPart, TraitId, TypeDef, TypeHead, TypeId,
    UnaryOp,
};

mod traits;

use crate::codes;
use crate::exhaustive;
use crate::ty::{Table, VarKind};
use crate::{ArgValue, BodyTypes, Method, Projection, TypeckResult};
use pika_ty::Ty;

/// The parameters and return type of a function, as seen by callers.
struct FnSig {
    params: Vec<ParamSig>,
    ret: Ty,
}

struct ParamSig {
    name: String,
    ty: Ty,
    has_default: bool,
}

/// The inference state of a module-level constant.
#[derive(Clone, Copy)]
enum ConstState {
    InProgress,
    Done(Ty),
}

pub(crate) struct ModuleChecker<'m> {
    module: &'m Module,
    sigs: ArenaMap<FnId, FnSig>,
    consts: ArenaMap<ConstId, ConstState>,
    result: TypeckResult,
}

impl<'m> ModuleChecker<'m> {
    pub(crate) fn new(module: &'m Module) -> Self {
        let mut sigs = ArenaMap::default();
        for (id, function) in module.functions.iter() {
            let params = function
                .params
                .iter()
                .map(|p| ParamSig {
                    name: function.body.locals[p.local].name.value.clone(),
                    ty: p.ty.value,
                    has_default: p.default.is_some(),
                })
                .collect();
            sigs.insert(
                id,
                FnSig {
                    params,
                    ret: function.ret.value,
                },
            );
        }
        Self {
            module,
            sigs,
            consts: ArenaMap::default(),
            result: TypeckResult::default(),
        }
    }

    pub(crate) fn run(mut self) -> TypeckResult {
        for (id, _) in self.module.traits.iter() {
            self.check_trait(id);
        }
        for (id, _) in self.module.types.iter() {
            self.check_type(id);
            self.check_impls(id);
        }
        for (id, _) in self.module.impls.iter() {
            self.check_builtin_impl(id);
        }
        for (id, _) in self.module.consts.iter() {
            self.const_ty(id);
        }
        for (id, global) in self.module.globals.iter() {
            let ty = global.ty.as_ref().map_or(Ty::Error, |t| t.value);
            if let Some(declared) = &global.ty {
                self.check_type_wf(declared.value, declared.span, &[]);
            }
            self.result.globals.insert(id, ty);
        }
        for (id, global) in self.module.globals.iter() {
            let ty = global.ty.as_ref().map_or(Ty::Error, |t| t.value);
            let mut ctx = InferCtx::new(&mut self, global.module, &global.body, Ty::Nothing, &[]);
            if let Some(init) = global.init {
                ctx.check_expr(init, Some(ty));
            }
            let types = ctx.finish();
            self.result.global_bodies.insert(id, types);
        }
        for (id, _) in self.module.functions.iter() {
            self.check_fn(id);
        }
        self.result
            .diagnostics
            .sort_by_key(|d| d.primary.span.start);
        self.result
    }

    fn report(&mut self, diagnostic: Diagnostic) {
        self.result.diagnostics.push(diagnostic);
    }

    /// The type of errors raised by `raises` functions.
    fn error_ty(&self) -> Ty {
        self.module
            .error_type
            .map_or(Ty::Error, |id| self.module.types[id].ty)
    }

    /// Reports, in a written type, map keys and set elements that cannot be keys, and type
    /// arguments that do not meet the bounds of their parameters. `generics` are the type
    /// parameters in scope.
    fn check_type_wf(&mut self, ty: Ty, span: Span, generics: &[GenericParam]) {
        let cx = Cx::new(self.module, generics);
        let key = match ty {
            Ty::Map(map) => Some(map.key),
            Ty::Set(element) => Some(*element),
            _ => None,
        };
        if let Some(id) = adt_of(self.module, ty) {
            let def = &self.module.types[id];
            for (param, arg) in def.generics.params.iter().zip(ty.components()) {
                for bound in &param.bounds {
                    if !satisfies(cx, arg, bound_requirement(bound.value)) {
                        self.report(bound_error(
                            self.module,
                            arg,
                            bound.value,
                            &format!("`{}`", def.name.value),
                            &param.name.value,
                            span,
                        ));
                    }
                }
            }
        }
        if let Some(key) = key
            && !satisfies(cx, key, Requirement::Key)
        {
            let help = match key {
                Ty::Param(param) => bound_hint(self.module, &param.name, Requirement::Key),
                _ => "keys must implement `Hash` and `Eq`: integers, `bool`, `char`, `String`, \
                      `Duration`, and types that derive them; floats cannot be keys"
                    .to_owned(),
            };
            self.report(
                Diagnostic::error(
                    codes::INVALID_KEY,
                    format!("`{key}` cannot be a map key or a set element"),
                    span,
                )
                .with_help(help),
            );
        }
        for component in ty.components() {
            self.check_type_wf(component, span, generics);
        }
    }

    /// The type of a module-level constant, inferring it on first use.
    fn const_ty(&mut self, id: ConstId) -> Ty {
        match self.consts.get(id) {
            Some(ConstState::Done(ty)) => return *ty,
            Some(ConstState::InProgress) => {
                let name = &self.module.consts[id].name;
                let diagnostic = Diagnostic::error(
                    codes::CYCLIC_CONSTANT,
                    format!("the value of constant `{}` depends on itself", name.value),
                    name.span,
                );
                self.report(diagnostic);
                self.consts.insert(id, ConstState::Done(Ty::Error));
                return Ty::Error;
            }
            None => {}
        }
        self.consts.insert(id, ConstState::InProgress);
        let item = &self.module.consts[id];
        let declared = item.ty.as_ref().map(|t| t.value);
        let mut ctx = InferCtx::new(self, item.module, &item.body, Ty::Nothing, &[]);
        if let Some(init) = item.init {
            ctx.check_expr(init, declared);
        }
        let types = ctx.finish();
        let inferred = declared
            .or_else(|| item.init.and_then(|init| types.exprs.get(init).copied()))
            .unwrap_or(Ty::Error);
        // A cycle through this constant has already recorded an error type.
        let ty = match self.consts.get(id) {
            Some(ConstState::Done(ty)) => *ty,
            _ => inferred,
        };
        self.consts.insert(id, ConstState::Done(ty));
        self.result.consts.insert(id, ty);
        self.result.const_bodies.insert(id, types);
        ty
    }

    /// Checks a type's field defaults, that it does not contain itself, and that its derives
    /// are valid for its fields.
    fn check_type(&mut self, id: TypeId) {
        let module = self.module;
        let def = &module.types[id];
        let mut ctx = InferCtx::new(
            self,
            def.module,
            &def.body,
            Ty::Nothing,
            &def.generics.params,
        );
        for field in &def.fields {
            if let Some(default) = field.default {
                ctx.check_expr(default, Some(field.ty.value));
            }
        }
        let types = ctx.finish();
        self.result.type_bodies.insert(id, types);
        for field in def
            .fields
            .iter()
            .chain(def.variants.iter().flat_map(|v| &v.fields))
        {
            self.check_type_wf(field.ty.value, field.ty.span, &def.generics.params);
        }

        if let Some(path) = self.contains_itself(id) {
            let fields: Vec<String> = path.iter().map(|name| format!("`{name}`")).collect();
            self.report(
                Diagnostic::error(
                    codes::RECURSIVE_STRUCT,
                    format!("type `{}` contains itself", def.name.value),
                    def.name.span,
                )
                .with_help(format!(
                    "through the field{} {}; a value cannot contain itself, so store it in a `Box`, as in `Box<{}>`",
                    if fields.len() == 1 { "" } else { "s" },
                    fields.join(", "),
                    def.name.value
                )),
            );
            return;
        }

        let derives = &def.derives;
        let checks = [
            (derives.copy, "Copy", Requirement::Copy),
            (derives.clone, "Clone", Requirement::Clone),
            (derives.eq, "Eq", Requirement::Eq),
            (derives.ord, "Ord", Requirement::Ord),
            (derives.display, "Display", Requirement::Display),
            (derives.hash, "Hash", Requirement::Hash),
            (derives.default, "Default", Requirement::Default),
        ];
        let all_fields = def
            .fields
            .iter()
            .chain(def.variants.iter().flat_map(|v| &v.fields));
        for (span, name, requirement) in checks {
            let Some(span) = span else { continue };
            if defines_trait(def, requirement) {
                continue;
            }
            if requirement == Requirement::Default && def.kind == AdtKind::Enum {
                self.report(
                    Diagnostic::error(
                        codes::INVALID_DERIVE,
                        format!(
                            "the enum `{}` cannot derive `Default`: it has no default variant",
                            def.name.value
                        ),
                        span,
                    )
                    .with_help(format!(
                        "define it: `:fn default -> Self do={{ :return {}->... }}`",
                        def.name.value
                    )),
                );
                continue;
            }
            // A field whose type has type parameters is checked for each use of the type; a
            // field with a default value needs no default from its type.
            for field in all_fields.clone().filter(|f| {
                !f.ty.value.has_params()
                    && (requirement != Requirement::Default || f.default.is_none())
            }) {
                if !satisfies(Cx::new(module, &[]), field.ty.value, requirement) {
                    self.report(
                        Diagnostic::error(
                            codes::INVALID_DERIVE,
                            format!(
                                "`{}` cannot implement `{name}`: field `{}` of type `{}` does not",
                                def.name.value, field.name.value, field.ty.value
                            ),
                            span,
                        )
                        .with_secondary(field.ty.span, format!("not `{name}`")),
                    );
                }
            }
        }
    }

    /// The names of the fields through which a type contains itself by value, if it does.
    /// A `Box` breaks the containment: it holds its value elsewhere.
    fn contains_itself(&self, id: TypeId) -> Option<Vec<String>> {
        fn visit(
            module: &Module,
            target: TypeId,
            current: TypeId,
            path: &mut Vec<String>,
            visited: &mut Vec<TypeId>,
        ) -> bool {
            let def = &module.types[current];
            let fields = def
                .fields
                .iter()
                .chain(def.variants.iter().flat_map(|v| &v.fields));
            for field in fields {
                for inner in contained_types(module, field.ty.value) {
                    path.push(field.name.value.clone());
                    if inner == target {
                        return true;
                    }
                    if !visited.contains(&inner) {
                        visited.push(inner);
                        if visit(module, target, inner, path, visited) {
                            return true;
                        }
                    }
                    path.pop();
                }
            }
            false
        }
        let mut path = Vec::new();
        visit(self.module, id, id, &mut path, &mut Vec::new()).then_some(path)
    }

    fn check_fn(&mut self, id: FnId) {
        let module = self.module;
        let function = &module.functions[id];
        let ret = function.ret.value;

        if module.entry == Some(id) && function.kind == FnKind::Declared {
            if let Some(param) = function.params.first() {
                let span = function.body.locals[param.local].name.span;
                self.report(
                    Diagnostic::error(codes::INVALID_MAIN, "`main` cannot take parameters", span)
                        .with_help(
                            "read command-line arguments with `/std/os/args` (planned for M6)",
                        ),
                );
            }
            if !matches!(ret, Ty::Nothing | Ty::Error) {
                self.report(Diagnostic::error(
                    codes::INVALID_MAIN,
                    "`main` cannot return a value",
                    function.ret.span,
                ));
            }
            if let Some(param) = function.generics.params.first() {
                self.report(Diagnostic::error(
                    codes::INVALID_MAIN,
                    "`main` cannot have type parameters",
                    param.name.span,
                ));
            }
        }

        let generics = &function.generics.params;
        for param in &function.params {
            self.check_type_wf(param.ty.value, param.ty.span, generics);
        }
        self.check_type_wf(ret, function.ret.span, generics);
        if !function.has_body {
            // A required function of a trait: only its signature.
            return;
        }
        // A closure's captured variables have the types of its parent's variables, which is
        // checked first.
        let capture_tys: Vec<(LocalId, Ty)> = match function.kind {
            FnKind::Closure(parent) => function
                .captures
                .iter()
                .map(|capture| {
                    let ty = self
                        .result
                        .functions
                        .get(parent)
                        .and_then(|types| types.locals.get(capture.outer).copied())
                        .unwrap_or(Ty::Error);
                    (capture.inner, ty)
                })
                .collect(),
            _ => Vec::new(),
        };
        let mut ctx = InferCtx::new(self, function.module, &function.body, ret, generics);
        for (local, ty) in capture_tys {
            ctx.local_tys.insert(local, ty);
        }
        ctx.in_function = true;
        ctx.raises = function.raises;
        for param in &function.params {
            let ty = param.ty.value;
            ctx.local_tys.insert(param.local, ty);
            if let Some(default) = param.default {
                ctx.check_expr(default, Some(ty));
            }
        }
        let diverges = ctx.check_block(&function.root);
        let types = ctx.finish();
        self.result.functions.insert(id, types);

        if diverges {
            return;
        }
        let end_label = function
            .root
            .span
            .map(|span| Span::new(span.end.saturating_sub(1), span.end));
        let message = match ret {
            Ty::Nothing | Ty::Error => return,
            Ty::Never => format!(
                "function `{}` returns `never` but can finish",
                function.name.value
            ),
            other => format!(
                "function `{}` can finish without returning a value of type `{other}`",
                function.name.value
            ),
        };
        let mut diagnostic = Diagnostic::error(codes::MISSING_RETURN, message, function.name.span);
        if let Some(end) = end_label {
            diagnostic = diagnostic.with_secondary(end, "the function can reach its end here");
        }
        self.report(diagnostic.with_help(
            "end every path with `:return value`, or with `:panic` if it cannot happen",
        ));
    }
}

// ----- Per-body inference ------------------------------------------------------------------

/// A requirement on a type that is checked once the type is known.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Requirement {
    /// Integer or float.
    Numeric,
    /// Integer.
    Integer,
    /// Signed integer, float or `Duration` (negation).
    Signed,
    /// Integer, float or `Duration` (`+` and `-`).
    Additive,
    /// Supports `=` and `!=`.
    Eq,
    /// Supports `<`, `>`, `<=` and `>=`.
    Ord,
    /// Can be printed and interpolated.
    Display,
    /// Can be converted with `as` to the given type.
    CastTo(Ty),
    /// Is copied instead of moved.
    Copy,
    /// Can be copied with `clone`.
    Clone,
    /// Can be hashed (`Hash`).
    Hash,
    /// Can be a key of a map or an element of a set: `Hash` and `Eq`.
    Key,
    /// Has a length: `:len`.
    Len,
    /// Has a default value: `[:default]`.
    Default,
    /// Implements a user-defined trait.
    Trait(TraitId),
}

/// The result of looking up a function of a type by name.
enum FnLookup {
    None,
    Found(FnId),
    /// Several traits have one; reported.
    Ambiguous,
}

/// How the type arguments of the owner of a called function are given.
#[derive(Clone, Copy)]
enum OwnerArgs<'a> {
    /// A method called on a value of this type, which binds to `self`.
    Receiver(Ty),
    /// A function called by name: the type arguments written after the type's name, and
    /// for a function of a trait, the type it is called on (`[Square->make]`, `[T->make]`).
    Written {
        args: Option<&'a Spanned<Vec<Ty>>>,
        self_ty: Option<Ty>,
    },
}

/// Where a requirement comes from, for the diagnostic.
#[derive(Clone, Copy, Debug)]
enum Origin {
    /// A value captured from a captured value, by a function value inside another.
    NestedCapture,
    /// The value of `$out->write`.
    Write,
    BinaryOp(BinaryOp),
    UnaryOp(UnaryOp),
    Builtin(Builtin),
    Interpolation,
    Cast,
    ForLoop,
    /// A map key or set element, in a type or a value.
    Key,
    /// A method of a collection, which needs its elements to support an operation.
    Method(&'static str),
    /// A bound of a type parameter of a function or type, by position.
    Bound(BoundItem, usize),
}

/// A generic declaration whose bounds a requirement comes from.
#[derive(Clone, Copy, Debug)]
enum BoundItem {
    Fn(FnId),
    Type(TypeId),
}

struct Obligation {
    ty: Ty,
    requirement: Requirement,
    origin: Origin,
    span: Span,
}

struct IntLiteral {
    expr: ExprId,
    value: u64,
    negative: bool,
}

struct LoopFrame {
    has_break: bool,
}

/// A `:match` whose exhaustiveness is checked once its types are known.
struct PendingMatch {
    scrutinee: ExprId,
    arms: Vec<(PatId, bool)>,
    /// Whether a pattern is invalid (already reported): the arms are then not checked, to
    /// avoid errors that follow from the first one.
    invalid: bool,
}

struct InferCtx<'c, 'm> {
    checker: &'c mut ModuleChecker<'m>,
    /// The module the body is in, which can use its private items.
    home: ModuleId,
    body: &'m Body,
    /// The type parameters in scope, with their bounds.
    generics: &'m [GenericParam],
    /// The type arguments of every call of a generic function.
    instances: ArenaMap<ExprId, Vec<Ty>>,
    ret: Ty,
    table: Table,
    expr_tys: ArenaMap<ExprId, Ty>,
    local_tys: ArenaMap<LocalId, Ty>,
    obligations: Vec<Obligation>,
    int_literals: Vec<IntLiteral>,
    float_literals: Vec<(ExprId, f64)>,
    loops: Vec<LoopFrame>,
    call_args: ArenaMap<ExprId, Vec<ArgValue>>,
    methods: ArenaMap<ExprId, Method>,
    projections: ArenaMap<ExprId, Projection>,
    /// Expressions whose value is wrapped in `some`, with the option type.
    wrapped: ArenaMap<ExprId, Ty>,
    pat_tys: ArenaMap<PatId, Ty>,
    pat_int_literals: Vec<(PatId, u64, bool)>,
    matches: Vec<PendingMatch>,
    /// Whether a function is being checked, rather than a module-level initializer (which
    /// cannot call functions at all).
    in_function: bool,
    /// Whether the function is declared `raises`: errors raised in it propagate.
    raises: bool,
    /// The number of `:onerror` bodies being checked, which catch errors raised in them.
    catching: usize,
    /// The calls whose `?` was checked against whether they raise.
    marks_checked: ArenaMap<ExprId, ()>,
}

/// The user-defined type a type is, if it is one.
fn adt_of(module: &Module, ty: Ty) -> Option<TypeId> {
    let Ty::Adt(adt) = ty else {
        return None;
    };
    ((adt.index as usize) < module.types.len())
        .then(|| TypeId::from_raw(la_arena::RawIdx::from_u32(adt.index)))
}

/// The struct a type is, if it is one.
fn struct_of(module: &Module, ty: Ty) -> Option<TypeId> {
    adt_of(module, ty).filter(|&id| module.types[id].kind == AdtKind::Struct)
}

/// The user-defined types that a value of `ty` contains by value: the type itself, or the
/// type in an option. A box holds its value elsewhere.
fn contained_types(module: &Module, ty: Ty) -> Vec<TypeId> {
    match ty {
        Ty::Option(inner) => contained_types(module, *inner),
        _ => adt_of(module, ty).into_iter().collect(),
    }
}

/// Whether values of `ty` can be copied with `[$value->clone]`.
fn is_clone(cx: Cx<'_>, ty: Ty) -> bool {
    satisfies(cx, ty, Requirement::Clone)
}

/// The type of a function as a value: `fn(params) -> ret`, with `raises` if it raises.
fn fn_value_ty(function: &Function) -> Ty {
    let params: Vec<Ty> = function.params.iter().map(|p| p.ty.value).collect();
    Ty::function(&params, function.ret.value, function.raises)
}

/// Whether function `id` is the `drop` of the `Drop` trait, or a type's version of it.
fn is_drop_function(module: &Module, id: FnId) -> bool {
    let function = &module.functions[id];
    let drop_trait = module.prelude_trait(PreludeTrait::Drop);
    match function.owner {
        Some(FnOwner::Trait(t)) => t == drop_trait,
        Some(FnOwner::Type(ty)) => {
            function.name.value == "drop"
                && module.types[ty]
                    .traits
                    .iter()
                    .any(|t| t.value == drop_trait)
        }
        // The functions of built-in types are not `drop`, which they do not implement.
        Some(FnOwner::Impl(_)) | None => false,
    }
}

/// How a function is written at a call: `:name`, or `Type->name` for the functions of a
/// struct.
fn callable_name(module: &Module, id: FnId) -> String {
    let function = &module.functions[id];
    match function.owner {
        Some(FnOwner::Type(owner)) => format!(
            "{}->{}",
            module.types[owner].name.value, function.name.value
        ),
        Some(FnOwner::Trait(owner)) => format!(
            "{}->{}",
            module.traits[owner].name.value, function.name.value
        ),
        Some(FnOwner::Impl(owner)) => format!(
            "{}->{}",
            module.impls[owner].self_ty.value, function.name.value
        ),
        None => format!(":{}", function.name.value),
    }
}

/// What decides whether a type meets a requirement: the module's types, and the type
/// parameters in scope with their bounds.
#[derive(Clone, Copy)]
struct Cx<'m> {
    module: &'m Module,
    generics: &'m [GenericParam],
}

impl<'m> Cx<'m> {
    fn new(module: &'m Module, generics: &'m [GenericParam]) -> Self {
        Self { module, generics }
    }
}

/// A bound and every bound it implies: the traits a user-defined trait requires,
/// transitively.
pub fn implied_bounds(module: &Module, bound: Bound) -> Vec<Bound> {
    let mut found = vec![bound];
    let mut next = 0;
    while let Some(&current) = found.get(next) {
        next += 1;
        if let Bound::Trait(id) = current {
            for supertrait in &module.traits[id].supertraits {
                if !found.contains(&supertrait.value) {
                    found.push(supertrait.value);
                }
            }
        }
    }
    found
}

/// Whether the types of a type parameter with `bound` meet `requirement`.
fn bound_meets(module: &Module, bound: Bound, requirement: Requirement) -> bool {
    implied_bounds(module, bound)
        .into_iter()
        .any(|implied| match implied {
            Bound::Builtin(builtin) => matches!(
                (builtin, requirement),
                (BuiltinTrait::Copy, Requirement::Copy | Requirement::Clone)
                    | (BuiltinTrait::Clone, Requirement::Clone)
                    | (BuiltinTrait::Eq, Requirement::Eq)
                    | (BuiltinTrait::Ord, Requirement::Ord | Requirement::Eq)
                    | (BuiltinTrait::Hash, Requirement::Hash)
                    | (BuiltinTrait::Display, Requirement::Display)
                    | (BuiltinTrait::Default, Requirement::Default)
            ),
            Bound::Trait(id) => requirement == Requirement::Trait(id),
        })
}

/// The requirement a bound places on the types of a type parameter.
fn bound_requirement(bound: Bound) -> Requirement {
    match bound {
        Bound::Builtin(BuiltinTrait::Copy) => Requirement::Copy,
        Bound::Builtin(BuiltinTrait::Clone) => Requirement::Clone,
        Bound::Builtin(BuiltinTrait::Eq) => Requirement::Eq,
        Bound::Builtin(BuiltinTrait::Ord) => Requirement::Ord,
        Bound::Builtin(BuiltinTrait::Hash) => Requirement::Hash,
        Bound::Builtin(BuiltinTrait::Display) => Requirement::Display,
        Bound::Builtin(BuiltinTrait::Default) => Requirement::Default,
        Bound::Trait(id) => Requirement::Trait(id),
    }
}

/// The user-defined traits a struct or enum implements, with the traits they require.
fn implemented_traits(module: &Module, id: TypeId) -> Vec<TraitId> {
    let mut traits = Vec::new();
    for listed in &module.types[id].traits {
        for bound in implied_bounds(module, Bound::Trait(listed.value)) {
            if let Bound::Trait(t) = bound
                && !traits.contains(&t)
            {
                traits.push(t);
            }
        }
    }
    traits
}

fn satisfies(cx: Cx<'_>, ty: Ty, requirement: Requirement) -> bool {
    satisfies_in(cx, ty, requirement, &mut Vec::new())
}

/// [`satisfies`], with the generic types being checked, for types that contain themselves
/// through a box: a requirement that depends on itself holds.
fn satisfies_in(
    cx: Cx<'_>,
    ty: Ty,
    requirement: Requirement,
    visiting: &mut Vec<(Ty, Requirement)>,
) -> bool {
    let module = cx.module;
    if matches!(ty, Ty::Error | Ty::Never) {
        return true;
    }
    if let Requirement::Key = requirement {
        return satisfies_in(cx, ty, Requirement::Hash, visiting)
            && satisfies_in(cx, ty, Requirement::Eq, visiting);
    }
    // Built-in types implement the prelude traits of the operators they support.
    if let Requirement::Trait(t) = requirement
        && let Some(prelude) = module.traits[t].prelude
        && !matches!(ty, Ty::Adt(_) | Ty::Param(_))
    {
        let builtin = match prelude {
            PreludeTrait::Add | PreludeTrait::Sub => Requirement::Additive,
            PreludeTrait::Mul | PreludeTrait::Div | PreludeTrait::Rem => Requirement::Numeric,
            PreludeTrait::Neg => Requirement::Signed,
            PreludeTrait::Concat => return matches!(ty, Ty::String | Ty::List(_)),
            PreludeTrait::Drop => return false,
        };
        return satisfies_in(cx, ty, builtin, visiting);
    }
    if let Ty::Option(inner) | Ty::Box(inner) = ty {
        let boxed = matches!(ty, Ty::Box(_));
        return match requirement {
            Requirement::Eq
            | Requirement::Ord
            | Requirement::Clone
            | Requirement::Display
            | Requirement::Hash => satisfies_in(cx, *inner, requirement, visiting),
            Requirement::Copy => !boxed && satisfies_in(cx, *inner, requirement, visiting),
            // `none`, or a box of the default value.
            Requirement::Default => !boxed || satisfies_in(cx, *inner, requirement, visiting),
            Requirement::CastTo(target) => target == ty || target == Ty::Error,
            _ => false,
        };
    }
    if let Ty::List(_) | Ty::Set(_) | Ty::Map(_) = ty {
        let mut all = |requirement| {
            ty.components()
                .into_iter()
                .all(|c| satisfies_in(cx, c, requirement, visiting))
        };
        return match requirement {
            Requirement::Eq | Requirement::Clone | Requirement::Display => all(requirement),
            // Sets and maps have no order of their own: only lists compare with `<`.
            Requirement::Ord => matches!(ty, Ty::List(_)) && all(requirement),
            Requirement::Len | Requirement::Default => true,
            Requirement::CastTo(target) => target == ty || target == Ty::Error,
            _ => false,
        };
    }
    if let Ty::Param(param) = ty {
        return cx.generics.get(param.index as usize).is_some_and(|p| {
            p.bounds
                .iter()
                .any(|b| bound_meets(module, b.value, requirement))
        });
    }
    if let Some(id) = adt_of(module, ty) {
        return adt_satisfies(cx, id, ty, requirement, visiting);
    }
    if let Some(met) = opaque_satisfies(ty, requirement) {
        return met;
    }
    match requirement {
        Requirement::Copy => ty != Ty::String,
        Requirement::Clone => !matches!(ty, Ty::Nothing | Ty::Var(_)),
        Requirement::Numeric => matches!(ty, Ty::Int(_) | Ty::Float(_)),
        Requirement::Integer => matches!(ty, Ty::Int(_)),
        Requirement::Signed => match ty {
            Ty::Int(int) => int.is_signed(),
            Ty::Float(_) | Ty::Duration => true,
            _ => false,
        },
        Requirement::Additive => matches!(ty, Ty::Int(_) | Ty::Float(_) | Ty::Duration),
        Requirement::Eq => !matches!(ty, Ty::Var(_)),
        Requirement::Ord => {
            matches!(
                ty,
                Ty::Int(_) | Ty::Float(_) | Ty::Char | Ty::String | Ty::Duration
            )
        }
        Requirement::Display => !matches!(ty, Ty::Nothing | Ty::Var(_)),
        Requirement::Hash => matches!(
            ty,
            Ty::Int(_) | Ty::Bool | Ty::Char | Ty::String | Ty::Duration | Ty::Nothing
        ),
        Requirement::Len => ty == Ty::String,
        Requirement::Key => unreachable!("handled above"),
        Requirement::Default => !matches!(ty, Ty::Var(_) | Ty::Never),
        Requirement::Trait(_) => false,
        Requirement::CastTo(target) => {
            let numeric = |t: Ty| matches!(t, Ty::Int(_) | Ty::Float(_));
            (numeric(ty) && numeric(target))
                || (ty == Ty::Char && target == Ty::Int(IntTy::U32))
                || target == Ty::Error
                || ty == target
        }
    }
}

/// [`satisfies_in`] for struct or enum `id`, of type `ty`.
fn adt_satisfies(
    cx: Cx<'_>,
    id: TypeId,
    ty: Ty,
    requirement: Requirement,
    visiting: &mut Vec<(Ty, Requirement)>,
) -> bool {
    let module = cx.module;
    if let Requirement::Trait(t) = requirement {
        // Like a derived trait, a trait that requires built-in traits holds for the type
        // arguments for which the type has them.
        return implemented_traits(module, id).contains(&t)
            && implied_bounds(module, Bound::Trait(t))
                .into_iter()
                .all(|bound| match bound {
                    Bound::Builtin(_) => satisfies_in(cx, ty, bound_requirement(bound), visiting),
                    Bound::Trait(_) => true,
                });
    }
    let def = &module.types[id];
    let derives = &def.derives;
    let has_trait = match requirement {
        Requirement::Eq => derives.eq.is_some() || derives.ord.is_some(),
        Requirement::Ord => derives.ord.is_some(),
        Requirement::Display => derives.display.is_some(),
        Requirement::CastTo(target) => return target == ty || target == Ty::Error,
        Requirement::Copy => derives.copy.is_some(),
        Requirement::Clone => derives.clone.is_some() || derives.copy.is_some(),
        Requirement::Hash => derives.hash.is_some(),
        Requirement::Default => derives.default.is_some(),
        _ => false,
    };
    // A trait the type defines itself holds for every type argument; a derived one, for the
    // type arguments whose fields have it.
    let args = ty.components();
    if !has_trait
        || args.is_empty()
        || defines_trait(def, requirement)
        || visiting.contains(&(ty, requirement))
    {
        return has_trait;
    }
    visiting.push((ty, requirement));
    let fields_meet = def
        .fields
        .iter()
        .chain(def.variants.iter().flat_map(|v| &v.fields))
        // A field with a default value of its own needs no default from its type.
        .filter(|f| !(requirement == Requirement::Default && f.default.is_some()))
        .all(|f| satisfies_in(cx, f.ty.value.subst(&args), requirement, visiting));
    visiting.pop();
    fields_meet
}

/// Whether a type defines a built-in trait with its own function, rather than deriving it:
/// `Default` with `default`, `Display` with `fmt`.
fn defines_trait(def: &TypeDef, requirement: Requirement) -> bool {
    match requirement {
        Requirement::Default => def.function("default").is_some(),
        Requirement::Display => def.function("fmt").is_some(),
        _ => false,
    }
}

/// For the built-in types with few traits: whether they meet `requirement`. A function value
/// can only be cloned (which shares what it captured) and called; a formatter only receives
/// text.
fn opaque_satisfies(ty: Ty, requirement: Requirement) -> Option<bool> {
    let only_itself =
        matches!(requirement, Requirement::CastTo(target) if target == ty || target == Ty::Error);
    match ty {
        Ty::Fn(_) => Some(requirement == Requirement::Clone || only_itself),
        Ty::Formatter => Some(only_itself),
        _ => None,
    }
}

/// A built-in method of a built-in type: what it is, its parameters and result, and what it
/// needs of the elements.
struct BuiltinMethod {
    name: &'static str,
    method: Method,
    params: Vec<(&'static str, Ty)>,
    ret: Ty,
    needs: Option<(Ty, Requirement)>,
}

/// A built-in method without requirements on the elements.
fn method(
    name: &'static str,
    method: Method,
    params: Vec<(&'static str, Ty)>,
    ret: Ty,
) -> BuiltinMethod {
    BuiltinMethod {
        name,
        method,
        params,
        ret,
        needs: None,
    }
}

/// A built-in method that needs its elements (of type `element`) to meet a requirement.
fn needing(mut found: BuiltinMethod, element: Ty, requirement: Requirement) -> BuiltinMethod {
    found.needs = Some((element, requirement));
    found
}

/// The built-in method `name` of values of type `ty`, if there is one.
fn builtin_method(cx: Cx<'_>, name: &str, ty: Ty) -> Option<BuiltinMethod> {
    let int = Ty::Int(IntTy::I64);
    Some(match (name, ty) {
        // Whether a type with unknown parts, like `Pair<{integer}>`, can be cloned is decided
        // once inference is done.
        ("clone", _) if ty.has_vars() => needing(
            method("clone", Method::Clone, vec![], ty),
            ty,
            Requirement::Clone,
        ),
        ("clone", _) if is_clone(cx, ty) => method("clone", Method::Clone, vec![], ty),
        ("is_some", Ty::Option(_)) => method("is_some", Method::IsSome, vec![], Ty::Bool),
        ("is_none", Ty::Option(_)) => method("is_none", Method::IsNone, vec![], Ty::Bool),
        ("unwrap", Ty::Option(inner)) => method("unwrap", Method::Unwrap, vec![], *inner),
        ("unwrap_or", Ty::Option(inner)) => method(
            "unwrap_or",
            Method::UnwrapOr,
            vec![("default", *inner)],
            *inner,
        ),
        ("take", Ty::Option(_)) => method("take", Method::Take, vec![], ty),
        ("unbox", Ty::Box(inner)) => method("unbox", Method::Unbox, vec![], *inner),
        ("len", Ty::List(_) | Ty::Map(_) | Ty::Set(_)) => method("len", Method::Len, vec![], int),
        ("is_empty", Ty::List(_) | Ty::Map(_) | Ty::Set(_)) => {
            method("is_empty", Method::IsEmpty, vec![], Ty::Bool)
        }
        ("clear", Ty::List(_) | Ty::Map(_) | Ty::Set(_)) => {
            method("clear", Method::Clear, vec![], Ty::Nothing)
        }
        (_, Ty::List(element)) => list_method(name, *element)?,
        (_, Ty::Map(map)) => map_method(name, map.key, map.value)?,
        (_, Ty::Set(element)) => set_method(name, *element)?,
        _ => return None,
    })
}

/// A method of `List<element>` other than those of every collection.
fn list_method(name: &str, element: Ty) -> Option<BuiltinMethod> {
    let int = Ty::Int(IntTy::I64);
    Some(match name {
        "push" => method("push", Method::Push, vec![("value", element)], Ty::Nothing),
        "pop" => method("pop", Method::Pop, vec![], Ty::option(element)),
        "insert" => method(
            "insert",
            Method::Insert,
            vec![("index", int), ("value", element)],
            Ty::Nothing,
        ),
        "remove" => method("remove", Method::Remove, vec![("index", int)], element),
        "swap" => method(
            "swap",
            Method::Swap,
            vec![("i", int), ("j", int)],
            Ty::Nothing,
        ),
        "get" => needing(
            method(
                "get",
                Method::Get,
                vec![("index", int)],
                Ty::option(element),
            ),
            element,
            Requirement::Clone,
        ),
        "contains" => needing(
            method(
                "contains",
                Method::Contains,
                vec![("value", element)],
                Ty::Bool,
            ),
            element,
            Requirement::Eq,
        ),
        _ => return None,
    })
}

/// A method of `Map<key, value>` other than those of every collection.
fn map_method(name: &str, key: Ty, value: Ty) -> Option<BuiltinMethod> {
    Some(match name {
        "insert" => method(
            "insert",
            Method::Insert,
            vec![("key", key), ("value", value)],
            Ty::option(value),
        ),
        "remove" => method(
            "remove",
            Method::Remove,
            vec![("key", key)],
            Ty::option(value),
        ),
        "get" => needing(
            method("get", Method::Get, vec![("key", key)], Ty::option(value)),
            value,
            Requirement::Clone,
        ),
        "contains" => method("contains", Method::Contains, vec![("key", key)], Ty::Bool),
        "keys" => needing(
            method("keys", Method::Keys, vec![], Ty::list(key)),
            key,
            Requirement::Clone,
        ),
        "values" => needing(
            method("values", Method::Values, vec![], Ty::list(value)),
            value,
            Requirement::Clone,
        ),
        _ => return None,
    })
}

/// A method of `Set<element>` other than those of every collection.
fn set_method(name: &str, element: Ty) -> Option<BuiltinMethod> {
    Some(match name {
        "insert" => method("insert", Method::Insert, vec![("value", element)], Ty::Bool),
        "remove" => method("remove", Method::Remove, vec![("value", element)], Ty::Bool),
        "contains" => method(
            "contains",
            Method::Contains,
            vec![("value", element)],
            Ty::Bool,
        ),
        _ => return None,
    })
}

/// The error for a type argument that does not meet a bound of its parameter.
fn bound_error(
    module: &Module,
    arg: Ty,
    bound: Bound,
    what: &str,
    param: &str,
    span: Span,
) -> Diagnostic {
    Diagnostic::error(
        codes::BOUND_NOT_MET,
        format!(
            "`{arg}` does not implement {}, which {what} requires of `{param}`",
            requirement_name(module, bound_requirement(bound))
        ),
        span,
    )
}

/// Advice for a type that does not meet a requirement.
fn requirement_help(module: &Module, ty: Ty, requirement: Requirement) -> Option<String> {
    if let (Requirement::Trait(id), Ty::Adt(adt)) = (requirement, ty) {
        let def = &module.traits[id];
        let help = format!("add `{}` to the `impl=` of `{}`", def.name.value, adt.name);
        return Some(match def.prelude {
            Some(prelude) => format!("{help} and define `{}` in it", prelude.signature()),
            None => help,
        });
    }
    let help = match requirement {
        Requirement::Additive if ty == Ty::String => {
            Some("strings are joined with `.`, as in `($a . $b)`")
        }
        Requirement::Signed if matches!(ty, Ty::Int(_)) => {
            Some("unsigned integers cannot be negative; convert to a signed type first")
        }
        Requirement::CastTo(_) => {
            Some("`as` converts between numeric types, and from `char` to `u32`")
        }
        Requirement::Len => Some("`:len` measures strings, lists, maps and sets"),
        Requirement::Key => Some(
            "keys must implement `Hash` and `Eq`: integers, `bool`, `char`, `String`, \
             `Duration`, and types that derive them; floats cannot be keys",
        ),
        _ => None,
    };
    match ty {
        Ty::Param(param) if is_trait(requirement) => {
            Some(bound_hint(module, &param.name, requirement))
        }
        _ => help.map(str::to_owned),
    }
}

/// Advice to add the bound that type parameter `name` lacks.
fn bound_hint(module: &Module, name: &str, requirement: Requirement) -> String {
    let bound = match requirement {
        Requirement::Key => "Hash + Eq".to_owned(),
        other => requirement_name(module, other).trim_matches('`').to_owned(),
    };
    format!("add a bound to the type parameter: `<{name}: {bound}>`")
}

/// Whether a requirement is a trait that a type parameter can be bound by.
fn is_trait(requirement: Requirement) -> bool {
    matches!(
        requirement,
        Requirement::Eq
            | Requirement::Ord
            | Requirement::Clone
            | Requirement::Display
            | Requirement::Hash
            | Requirement::Key
            | Requirement::Copy
            | Requirement::Default
            | Requirement::Trait(_)
    )
}

/// The trait a requirement is, for messages, in backquotes.
fn requirement_name(module: &Module, requirement: Requirement) -> String {
    match requirement {
        Requirement::Eq => "`Eq`".to_owned(),
        Requirement::Ord => "`Ord`".to_owned(),
        Requirement::Clone => "`Clone`".to_owned(),
        Requirement::Display => "`Display`".to_owned(),
        Requirement::Hash => "`Hash`".to_owned(),
        Requirement::Key => "`Hash` and `Eq`".to_owned(),
        Requirement::Copy => "`Copy`".to_owned(),
        Requirement::Default => "`Default`".to_owned(),
        Requirement::Trait(id) => format!("`{}`", module.traits[id].name.value),
        _ => "suitable".to_owned(),
    }
}

impl<'c, 'm> InferCtx<'c, 'm> {
    fn new(
        checker: &'c mut ModuleChecker<'m>,
        home: ModuleId,
        body: &'m Body,
        ret: Ty,
        generics: &'m [GenericParam],
    ) -> Self {
        Self {
            checker,
            home,
            body,
            generics,
            instances: ArenaMap::default(),
            ret,
            table: Table::default(),
            expr_tys: ArenaMap::default(),
            local_tys: ArenaMap::default(),
            obligations: Vec::new(),
            int_literals: Vec::new(),
            float_literals: Vec::new(),
            loops: Vec::new(),
            call_args: ArenaMap::default(),
            methods: ArenaMap::default(),
            projections: ArenaMap::default(),
            wrapped: ArenaMap::default(),
            pat_tys: ArenaMap::default(),
            pat_int_literals: Vec::new(),
            matches: Vec::new(),
            in_function: false,
            raises: false,
            catching: 0,
            marks_checked: ArenaMap::default(),
        }
    }

    fn report(&mut self, diagnostic: Diagnostic) {
        self.checker.report(diagnostic);
    }

    /// Reports the use, at `span`, of `name`, a field or function of the type or trait `owner`
    /// declared in module `module`, if it is private to that module and used outside it.
    fn check_visible(&mut self, module: ModuleId, owner: &str, name: &str, what: &str, span: Span) {
        if module == self.home || !pika_hir::is_private(name) {
            return;
        }
        self.report(
            Diagnostic::error(
                pika_hir::codes::PRIVATE_ITEM,
                format!("the {what} `{name}` of `{owner}` is private to its module"),
                span,
            )
            .with_help(
                "names that start with `_` can only be used in the module that declares them",
            ),
        );
    }

    /// What decides requirements here: the module, and the type parameters in scope.
    fn cx(&self) -> Cx<'m> {
        Cx::new(self.checker.module, self.generics)
    }

    fn span(&self, expr: ExprId) -> Span {
        self.body.expr_span(expr)
    }

    fn show(&self, ty: Ty) -> String {
        format!("`{}`", self.table.display(ty))
    }

    // ----- Unification ---------------------------------------------------------------------

    /// Checks that `actual` (the type of `expr`) fits `expected`, reporting a mismatch.
    fn coerce(&mut self, expr: ExprId, actual: Ty, expected: Ty) -> Ty {
        if self.table.unify(actual, expected).is_ok() {
            // Keep `never` so that callers can see that the expression does not finish.
            return if self.table.resolve(actual) == Ty::Never {
                Ty::Never
            } else {
                expected
            };
        }
        // Option wrapping, the one implicit conversion (spec section 5.4): a `T` is accepted
        // where a `T?` is expected.
        if let Ty::Option(inner) = self.table.resolve(expected)
            && !matches!(self.table.resolve(actual), Ty::Option(_))
            && self.table.unify(actual, *inner).is_ok()
        {
            self.wrapped.insert(expr, expected);
            return expected;
        }
        let (shown_expected, shown_actual) = (self.show(expected), self.show(actual));
        let mut diagnostic = Diagnostic::error(
            codes::MISMATCHED_TYPES,
            format!("mismatched types: expected {shown_expected}, found {shown_actual}"),
            self.span(expr),
        )
        .with_label(format!("expected {shown_expected}"));
        let (expected, actual) = (self.table.resolve(expected), self.table.resolve(actual));
        let var_kind = |ty: Ty| match ty {
            Ty::Var(var) => Some(self.table.kind(var)),
            _ => None,
        };
        let is_float = |ty: Ty| matches!(ty, Ty::Float(_)) || var_kind(ty) == Some(VarKind::Float);
        if var_kind(actual) == Some(VarKind::Int) && is_float(expected) {
            diagnostic = diagnostic.with_help("write the number with a decimal point, as in `5.0`");
        } else if var_kind(expected) == Some(VarKind::Int) && is_float(actual) {
            diagnostic = diagnostic.with_help(
                "integers and floats cannot be mixed; write integer literals with a decimal point, \
                 as in `1.0`, or convert with `as`",
            );
        } else if satisfies(self.cx(), expected, Requirement::Numeric)
            && matches!(actual, Ty::Int(_) | Ty::Float(_))
            && matches!(expected, Ty::Int(_) | Ty::Float(_))
        {
            diagnostic = diagnostic.with_help(format!(
                "Pika never converts numbers implicitly; convert with `as`, as in `(value as {expected})`"
            ));
        }
        self.report(diagnostic);
        Ty::Error
    }

    /// Records a requirement, checking it now if the type is known.
    fn require(&mut self, ty: Ty, requirement: Requirement, origin: Origin, span: Span) {
        let resolved = self.table.resolve(ty);
        if let Ty::Var(var) = resolved {
            let decided = match (self.table.kind(var), requirement) {
                (VarKind::Int, Requirement::Signed) | (VarKind::General, _) => None,
                (VarKind::Int, req) => Some(satisfies(self.cx(), Ty::Int(IntTy::I64), req)),
                (VarKind::Float, req) => Some(satisfies(self.cx(), Ty::Float(FloatTy::F64), req)),
            };
            match decided {
                Some(true) => {}
                Some(false) => self.report_requirement(resolved, requirement, origin, span),
                None => self.obligations.push(Obligation {
                    ty,
                    requirement,
                    origin,
                    span,
                }),
            }
            return;
        }
        // A type with unknown parts, like `{integer}?`, is checked once inference is done.
        let resolved = self.table.resolve_deep(resolved);
        if resolved.has_vars() {
            self.obligations.push(Obligation {
                ty,
                requirement,
                origin,
                span,
            });
            return;
        }
        if !satisfies(self.cx(), resolved, requirement) {
            self.report_requirement(resolved, requirement, origin, span);
        }
    }

    fn report_requirement(&mut self, ty: Ty, requirement: Requirement, origin: Origin, span: Span) {
        let shown = self.show(ty);
        let (code, message) = match origin {
            Origin::BinaryOp(op) => (
                codes::UNSUPPORTED_OPERATION,
                format!("operator `{}` cannot be used with {shown}", op.symbol()),
            ),
            Origin::UnaryOp(op) => {
                let symbol = match op {
                    UnaryOp::Neg => "-",
                    UnaryOp::Not => "!",
                    UnaryOp::BitNot => "~",
                };
                (
                    codes::UNSUPPORTED_OPERATION,
                    format!("operator `{symbol}` cannot be used with {shown}"),
                )
            }
            Origin::Builtin(Builtin::Default) => (
                codes::UNSUPPORTED_OPERATION,
                format!("{shown} has no default value"),
            ),
            Origin::Builtin(builtin) => (
                codes::UNSUPPORTED_OPERATION,
                format!(
                    "`:{}` cannot be used with a value of type {shown}",
                    builtin.name()
                ),
            ),
            Origin::NestedCapture => (
                codes::UNSUPPORTED_OPERATION,
                format!(
                    "this function value captures a copy of a captured value of type {shown}, which is not `Clone`"
                ),
            ),
            Origin::Write => (
                codes::UNSUPPORTED_OPERATION,
                format!("`write` cannot be used with a value of type {shown}"),
            ),
            Origin::Interpolation => (
                codes::UNSUPPORTED_OPERATION,
                format!("a value of type {shown} cannot be interpolated into a string"),
            ),
            Origin::Cast => {
                let target = match requirement {
                    Requirement::CastTo(target) => self.show(target),
                    _ => unreachable!("casts only require `CastTo`"),
                };
                (
                    codes::INVALID_CAST,
                    format!("cannot convert {shown} to {target} with `as`"),
                )
            }
            Origin::ForLoop => (
                codes::UNSUPPORTED_OPERATION,
                format!("`:for` loops count with integers, not {shown}"),
            ),
            Origin::Key => (
                codes::INVALID_KEY,
                format!("{shown} cannot be a map key or a set element"),
            ),
            Origin::Bound(item, param) => {
                let module = self.checker.module;
                let (what, generics) = match item {
                    BoundItem::Fn(id) => (
                        format!("`{}`", callable_name(module, id)),
                        &module.functions[id].generics,
                    ),
                    BoundItem::Type(id) => (
                        format!("`{}`", module.types[id].name.value),
                        &module.types[id].generics,
                    ),
                };
                let name = generics
                    .params
                    .get(param)
                    .map_or("_", |p| p.name.value.as_str());
                (
                    codes::BOUND_NOT_MET,
                    format!(
                        "{shown} does not implement {}, which {what} requires of `{name}`",
                        requirement_name(module, requirement)
                    ),
                )
            }
            Origin::Method(method) => (
                codes::UNSUPPORTED_OPERATION,
                format!(
                    "`{method}` needs elements that are {}, but they are {shown}",
                    requirement_name(self.checker.module, requirement)
                ),
            ),
        };
        let help = requirement_help(self.checker.module, ty, requirement)
            .or_else(|| self.unmet_supertrait(ty, requirement));
        let mut diagnostic = Diagnostic::error(code, message, span);
        if let Some(help) = help {
            diagnostic = diagnostic.with_help(help);
        }
        self.report(diagnostic);
    }

    // ----- Expressions ---------------------------------------------------------------------

    fn infer_expr(&mut self, expr: ExprId) -> Ty {
        self.check_expr(expr, None)
    }

    /// Infers the type of `expr`, checking it against `expected` if given.
    fn check_expr(&mut self, expr: ExprId, expected: Option<Ty>) -> Ty {
        let actual = self.infer_inner(expr, expected);
        let ty = match expected {
            Some(expected) => self.coerce(expr, actual, expected),
            None => actual,
        };
        // A wrapped value keeps its own type; the option is recorded separately.
        let own = if self.wrapped.get(expr).is_some() {
            actual
        } else {
            ty
        };
        self.expr_tys.insert(expr, own);
        ty
    }

    /// A string with interpolated values, which must be `Display`.
    fn infer_string(&mut self, parts: &[StringPart]) -> Ty {
        for part in parts {
            if let StringPart::Expr(part) = *part {
                let ty = self.infer_expr(part);
                let span = self.span(part);
                self.require(ty, Requirement::Display, Origin::Interpolation, span);
            }
        }
        Ty::String
    }

    fn infer_inner(&mut self, expr: ExprId, expected: Option<Ty>) -> Ty {
        let body = self.body;
        match &body.exprs[expr] {
            Expr::Missing => Ty::Error,
            Expr::Literal(literal) => self.infer_literal(expr, literal),
            Expr::String(parts) => self.infer_string(parts),
            Expr::Local(local) => self.local_ty(*local),
            Expr::Const(id) => self.checker.const_ty(*id),
            Expr::Global(id) => self
                .checker
                .result
                .globals
                .get(*id)
                .copied()
                .unwrap_or(Ty::Error),
            Expr::Call {
                callee,
                args,
                owner_args,
                fn_args,
                self_ty,
            } => {
                let owner = OwnerArgs::Written {
                    args: owner_args.as_ref(),
                    self_ty: *self_ty,
                };
                match callee {
                    Callee::Builtin(Builtin::Default) => {
                        self.infer_default(expr, args, fn_args.as_ref(), expected)
                    }
                    _ => self.infer_call(expr, *callee, args, owner, fn_args.as_ref()),
                }
            }
            Expr::MethodCall {
                receiver,
                method,
                args,
                type_args,
            } => self.infer_method_call(expr, *receiver, method, args, type_args.as_ref()),
            Expr::StructLit {
                strukt,
                fields,
                type_args,
            } => self.infer_struct_lit(expr, *strukt, fields, type_args.as_ref(), expected),
            Expr::Field { base, field } => self.infer_field(expr, *base, field),
            Expr::Variant {
                ty,
                variant,
                args,
                type_args,
            } => self.infer_variant(expr, *ty, *variant, args, type_args.as_ref(), expected),
            Expr::Collection { declared, elements } => {
                self.infer_collection(expr, declared.as_ref(), elements, expected)
            }
            &Expr::Index { base, index } => self.infer_index(expr, base, index),
            Expr::Some(_) | Expr::None | Expr::BoxNew(_) => self.infer_wrapper(expr, expected),
            &Expr::Raise { value, source } => self.infer_raise(expr, value, source),
            &Expr::Closure(id) => self.infer_closure(expr, id),
            &Expr::FnRef(id) => self.infer_fn_ref(expr, id),
            Expr::CallValue { callee, args } => self.infer_value_call(expr, *callee, args),
            Expr::Unary { op, operand } => self.infer_unary(*op, *operand, expected, expr),
            &Expr::Binary {
                op,
                op_span,
                lhs,
                rhs,
            } => self.infer_binary(expr, op, op_span, lhs, rhs),
            Expr::Cast { expr: inner, ty } => {
                let source = self.infer_expr(*inner);
                let target = ty.value;
                self.require(
                    source,
                    Requirement::CastTo(target),
                    Origin::Cast,
                    self.span(expr),
                );
                target
            }
            Expr::Return(value) => {
                self.infer_return(expr, *value);
                Ty::Never
            }
            // Outside a loop (already reported), these do not affect the control flow.
            Expr::Break => match self.loops.last_mut() {
                Some(frame) => {
                    frame.has_break = true;
                    Ty::Never
                }
                None => Ty::Error,
            },
            Expr::Continue => {
                if self.loops.is_empty() {
                    Ty::Error
                } else {
                    Ty::Never
                }
            }
        }
    }

    /// A struct literal: `Name{field=value; ...}`.
    fn infer_struct_lit(
        &mut self,
        expr: ExprId,
        strukt: TypeId,
        fields: &[Option<ExprId>],
        type_args: Option<&Spanned<Vec<Ty>>>,
        expected: Option<Ty>,
    ) -> Ty {
        let module = self.checker.module;
        let def = &module.types[strukt];
        let args = self.instantiate(
            &def.generics.params,
            type_args,
            BoundItem::Type(strukt),
            self.span(expr),
            expected,
        );
        for (field, value) in def.fields.iter().zip(fields) {
            if let Some(value) = value {
                let span = self.span(*value);
                self.check_visible(
                    def.module,
                    &def.name.value,
                    &field.name.value,
                    "field",
                    span,
                );
                self.check_expr(*value, Some(field.ty.value.subst(&args)));
            }
        }
        def.ty.subst(&args)
    }

    fn infer_literal(&mut self, expr: ExprId, literal: &Literal) -> Ty {
        match *literal {
            Literal::Int { value, negative } => {
                self.int_literals.push(IntLiteral {
                    expr,
                    value,
                    negative,
                });
                self.table.new_var(VarKind::Int)
            }
            Literal::Float(value) => {
                self.float_literals.push((expr, value));
                self.table.new_var(VarKind::Float)
            }
            Literal::Bool(_) => Ty::Bool,
            Literal::Char(_) => Ty::Char,
            Literal::Duration(_) => Ty::Duration,
        }
    }

    /// Checks `:return value?` against the function's return type.
    fn infer_return(&mut self, expr: ExprId, value: Option<ExprId>) {
        let ret = self.ret;
        let returns_nothing = self.table.resolve(ret) == Ty::Nothing;
        match value {
            Some(value) if returns_nothing => {
                let ty = self.infer_expr(value);
                if !matches!(self.table.resolve(ty), Ty::Nothing | Ty::Never | Ty::Error) {
                    let shown = self.show(ty);
                    self.report(
                        Diagnostic::error(
                            codes::MISMATCHED_TYPES,
                            format!(
                                "this function does not return a value, but a {shown} is returned"
                            ),
                            self.span(value),
                        )
                        .with_help("declare the return type after the parameters, as in `-> i64`"),
                    );
                }
            }
            Some(value) => {
                self.check_expr(value, Some(ret));
            }
            None if !matches!(self.table.resolve(ret), Ty::Nothing | Ty::Error) => {
                let shown = self.show(ret);
                self.report(
                    Diagnostic::error(
                        codes::MISSING_RETURN_VALUE,
                        format!("`:return` needs a value of type {shown}"),
                        self.span(expr),
                    )
                    .with_help("write `:return value`"),
                );
            }
            None => {}
        }
    }

    fn infer_field(&mut self, expr: ExprId, base: ExprId, field: &Spanned<String>) -> Ty {
        let base_ty = self.infer_expr(base);
        let resolved = self.table.resolve(base_ty);
        if matches!(resolved, Ty::Error) {
            return Ty::Error;
        }
        let module = self.checker.module;
        if let Ty::Box(inner) = resolved {
            if field.value == "value" {
                self.projections.insert(expr, Projection::BoxValue);
                return *inner;
            }
            self.report(
                Diagnostic::error(
                    codes::UNKNOWN_FIELD,
                    format!("`Box` has no field named `{}`", field.value),
                    field.span,
                )
                .with_help("the boxed value is `$b->value`"),
            );
            return Ty::Error;
        }
        let Some(strukt) = struct_of(module, resolved) else {
            let shown = self.show(resolved);
            let mut diagnostic = Diagnostic::error(
                codes::UNKNOWN_FIELD,
                format!("{shown} has no fields"),
                field.span,
            );
            if matches!(resolved, Ty::Option(_)) || adt_of(module, resolved).is_some() {
                diagnostic = diagnostic.with_help("read the fields of a variant with `:match`");
            }
            self.report(diagnostic);
            return Ty::Error;
        };
        let def = &module.types[strukt];
        let Some((index, found)) = def.field(&field.value) else {
            let names: Vec<String> = def
                .fields
                .iter()
                .map(|f| format!("`{}`", f.name.value))
                .collect();
            let mut diagnostic = Diagnostic::error(
                codes::UNKNOWN_FIELD,
                format!("`{}` has no field named `{}`", def.name.value, field.value),
                field.span,
            );
            if def.function(&field.value).is_some() {
                diagnostic = diagnostic.with_help(format!(
                    "`{}` is a method; call it as `[$value->{}]`",
                    field.value, field.value
                ));
            } else if !names.is_empty() {
                diagnostic = diagnostic.with_help(format!(
                    "the fields of `{}` are {}",
                    def.name.value,
                    names.join(", ")
                ));
            }
            self.report(diagnostic);
            return Ty::Error;
        };
        let found_ty = found.ty.value;
        let (home, owner) = (def.module, def.name.value.clone());
        self.check_visible(home, &owner, &field.value, "field", field.span);
        self.projections.insert(expr, Projection::Field(index));
        found_ty.subst(&resolved.components())
    }

    /// `[some value]`, `none` or `[Box->new value]`, whose value's type comes from the
    /// expected type if there is one.
    fn infer_wrapper(&mut self, expr: ExprId, expected: Option<Ty>) -> Ty {
        let expected = expected.map(|e| self.table.resolve(e));
        match (&self.body.exprs[expr], expected) {
            (&Expr::Some(value), Some(Ty::Option(inner))) => {
                Ty::option(self.check_expr(value, Some(*inner)))
            }
            (&Expr::Some(value), _) => Ty::option(self.infer_expr(value)),
            (Expr::None, Some(option @ Ty::Option(_))) => option,
            (Expr::None, _) => Ty::option(self.table.new_var(VarKind::General)),
            (&Expr::BoxNew(value), Some(Ty::Box(inner))) => {
                Ty::boxed(self.check_expr(value, Some(*inner)))
            }
            (&Expr::BoxNew(value), _) => Ty::boxed(self.infer_expr(value)),
            _ => unreachable!("not a wrapper"),
        }
    }

    /// A collection literal: a list, set or map, decided by the written type, the expected
    /// type, or the elements.
    fn infer_collection(
        &mut self,
        expr: ExprId,
        declared: Option<&Spanned<Ty>>,
        elements: &[Element],
        expected: Option<Ty>,
    ) -> Ty {
        let span = self.span(expr);
        let expected = expected.map(|e| self.table.resolve(e));
        // A written type, here or where the expected type comes from, has its keys checked
        // there; keys inferred from the elements are checked here.
        let written =
            declared.is_some() || matches!(expected, Some(Ty::List(_) | Ty::Set(_) | Ty::Map(_)));
        let ty = match (declared, expected) {
            (Some(declared), _) => {
                self.checker
                    .check_type_wf(declared.value, declared.span, self.generics);
                declared.value
            }
            (None, Some(ty @ (Ty::List(_) | Ty::Set(_) | Ty::Map(_)))) => ty,
            (None, _) => match elements.first() {
                Some(Element::Entry(..)) => Ty::map_of(
                    self.table.new_var(VarKind::General),
                    self.table.new_var(VarKind::General),
                ),
                Some(Element::Value(_)) => Ty::list(self.table.new_var(VarKind::General)),
                None => {
                    self.report(
                        Diagnostic::error(
                            codes::CANNOT_INFER,
                            "cannot infer the type of an empty collection",
                            span,
                        )
                        .with_help("write its type: `List<i64>{}`, `Set<String>{}` or `Map<String, i64>{}`"),
                    );
                    return Ty::Error;
                }
            },
        };
        let (key, value) = match ty {
            Ty::List(element) | Ty::Set(element) => (None, *element),
            Ty::Map(map) => (Some(map.key), map.value),
            _ => {
                for element in elements {
                    match *element {
                        Element::Value(value) => {
                            self.infer_expr(value);
                        }
                        Element::Entry(key, value) => {
                            self.infer_expr(key);
                            self.infer_expr(value);
                        }
                    }
                }
                return ty;
            }
        };
        let checked_key = match ty {
            Ty::Set(element) => Some(*element),
            _ => key,
        };
        if let Some(key) = checked_key.filter(|_| !written) {
            self.require(key, Requirement::Key, Origin::Key, span);
        }
        for element in elements {
            match (*element, key) {
                (Element::Value(element), None) => {
                    self.check_expr(element, Some(value));
                }
                (Element::Entry(entry_key, entry_value), Some(key)) => {
                    self.check_expr(entry_key, Some(key));
                    self.check_expr(entry_value, Some(value));
                }
                (Element::Value(element), Some(_)) => {
                    self.report(
                        Diagnostic::error(
                            codes::MISMATCHED_TYPES,
                            "a map literal lists `key=value` entries",
                            self.span(element),
                        )
                        .with_help("write the entry as `key=value`"),
                    );
                    self.infer_expr(element);
                }
                (Element::Entry(entry_key, entry_value), None) => {
                    let shown = self.show(ty);
                    self.report(Diagnostic::error(
                        codes::MISMATCHED_TYPES,
                        format!("a {shown} literal lists values, not `key=value` entries"),
                        self.span(entry_key),
                    ));
                    self.infer_expr(entry_key);
                    self.infer_expr(entry_value);
                }
            }
        }
        ty
    }

    /// `$list->index` or `$map->key`.
    fn infer_index(&mut self, expr: ExprId, base: ExprId, index: ExprId) -> Ty {
        let base_ty = self.infer_expr(base);
        match self.table.resolve(base_ty) {
            Ty::List(element) => {
                self.check_expr(index, Some(Ty::Int(IntTy::I64)));
                *element
            }
            Ty::Map(map) => {
                self.check_expr(index, Some(map.key));
                map.value
            }
            Ty::Error => {
                self.infer_expr(index);
                Ty::Error
            }
            other => {
                self.infer_expr(index);
                let shown = self.show(other);
                let mut diagnostic = Diagnostic::error(
                    codes::NOT_A_COLLECTION,
                    format!("{shown} cannot be indexed"),
                    self.span(expr),
                );
                diagnostic = match other {
                    Ty::String => diagnostic.with_help(
                        "strings are not indexable (planned: `[$s->chars]` and `[$s->bytes]`)",
                    ),
                    Ty::Set(_) => diagnostic.with_help("test membership with `($value in $set)`"),
                    Ty::Var(_) => diagnostic.with_help("add a type annotation to the collection"),
                    _ => diagnostic.with_help("only lists and maps are indexed"),
                };
                self.report(diagnostic);
                Ty::Error
            }
        }
    }

    /// `Enum->variant` or `[Enum->variant args]`.
    fn infer_variant(
        &mut self,
        expr: ExprId,
        ty: TypeId,
        variant: usize,
        args: &[CallArg],
        type_args: Option<&Spanned<Vec<Ty>>>,
        expected: Option<Ty>,
    ) -> Ty {
        let module = self.checker.module;
        let def = &module.types[ty];
        let instance = self.instantiate(
            &def.generics.params,
            type_args,
            BoundItem::Type(ty),
            self.span(expr),
            expected,
        );
        let fields = &def.variants[variant].fields;
        let name = format!("{}->{}", def.name.value, def.variants[variant].name.value);
        // A variant with fields used as a value was reported when lowering.
        if !args.is_empty() || fields.is_empty() {
            let names: Vec<String> = fields.iter().map(|f| f.name.value.clone()).collect();
            let tys: Vec<Ty> = fields.iter().map(|f| f.ty.value.subst(&instance)).collect();
            self.bind_args(expr, &name, &names, &tys, &vec![false; tys.len()], args);
        }
        def.ty.subst(&instance)
    }

    /// The type arguments of a use of a generic declaration: the written ones, or new type
    /// variables to infer. Each must meet the bounds of its parameter.
    ///
    /// Without written arguments, an `expected` instance of the same type gives them: they
    /// met the bounds where that type was written or instantiated.
    fn instantiate(
        &mut self,
        generics: &[GenericParam],
        written: Option<&Spanned<Vec<Ty>>>,
        item: BoundItem,
        span: Span,
        expected: Option<Ty>,
    ) -> Vec<Ty> {
        if written.is_none()
            && let BoundItem::Type(id) = item
            && let Some(expected @ Ty::Adt(adt)) = expected.map(|e| self.table.resolve(e))
            && adt_of(self.checker.module, expected) == Some(id)
            && adt.args.len() == generics.len()
        {
            return adt.args.to_vec();
        }
        let args = match written {
            Some(written) if written.value.len() == generics.len() => written.value.clone(),
            _ => (0..generics.len())
                .map(|_| self.table.new_var(VarKind::General))
                .collect(),
        };
        self.require_bounds(generics, &args, item, span);
        args
    }

    /// Requires each type argument to meet the bounds of its parameter.
    fn require_bounds(
        &mut self,
        generics: &[GenericParam],
        args: &[Ty],
        item: BoundItem,
        span: Span,
    ) {
        for (index, (param, &arg)) in generics.iter().zip(args).enumerate() {
            for bound in &param.bounds {
                self.require(
                    arg,
                    bound_requirement(bound.value),
                    Origin::Bound(item, index),
                    span,
                );
            }
        }
    }

    fn local_ty(&mut self, local: LocalId) -> Ty {
        if let Some(&ty) = self.local_tys.get(local) {
            return ty;
        }
        // Only reachable after an earlier error lowered a declaration away.
        let var = self.table.new_var(VarKind::General);
        self.local_tys.insert(local, var);
        var
    }

    fn infer_unary(
        &mut self,
        op: UnaryOp,
        operand: ExprId,
        expected: Option<Ty>,
        expr: ExprId,
    ) -> Ty {
        let span = self.span(expr);
        match op {
            UnaryOp::Neg => {
                let ty = self.check_expr(operand, expected);
                if self.operator_call(expr, PreludeTrait::Neg, ty) {
                    return ty;
                }
                let resolved = self.table.resolve(ty);
                if let Ty::Adt(_) | Ty::Param(_) = resolved {
                    let neg = self.checker.module.prelude_trait(PreludeTrait::Neg);
                    self.report_requirement(
                        resolved,
                        Requirement::Trait(neg),
                        Origin::UnaryOp(op),
                        span,
                    );
                    return Ty::Error;
                }
                self.require(ty, Requirement::Signed, Origin::UnaryOp(op), span);
                ty
            }
            UnaryOp::Not => {
                self.check_expr(operand, Some(Ty::Bool));
                Ty::Bool
            }
            UnaryOp::BitNot => {
                let ty = self.check_expr(operand, expected);
                self.require(ty, Requirement::Integer, Origin::UnaryOp(op), span);
                ty
            }
        }
    }

    /// For an operator applied to a value of type `ty`, a struct, enum or type parameter that
    /// implements the operator's prelude trait: records the call of the trait's function as
    /// the expression's method, and returns true.
    fn operator_call(&mut self, expr: ExprId, which: PreludeTrait, ty: Ty) -> bool {
        let resolved = self.table.resolve_deep(ty);
        if !matches!(resolved, Ty::Adt(_) | Ty::Param(_)) {
            return false;
        }
        let module = self.checker.module;
        let id = module.prelude_trait(which);
        if !satisfies(self.cx(), resolved, Requirement::Trait(id)) {
            return false;
        }
        let (_, function) = module.traits[id].functions[0];
        self.methods.insert(expr, Method::Fn(function));
        self.instances.insert(expr, vec![resolved]);
        true
    }

    fn infer_binary(
        &mut self,
        expr: ExprId,
        op: BinaryOp,
        op_span: Span,
        lhs: ExprId,
        rhs: ExprId,
    ) -> Ty {
        let origin = Origin::BinaryOp(op);
        if let Some(which) = PreludeTrait::of_operator(op) {
            let ty = self.infer_expr(lhs);
            if self.operator_call(expr, which, ty) {
                return self.check_expr(rhs, Some(ty));
            }
            let resolved = self.table.resolve(ty);
            if matches!(resolved, Ty::Adt(_) | Ty::Param(_)) {
                let id = self.checker.module.prelude_trait(which);
                self.report_requirement(resolved, Requirement::Trait(id), origin, op_span);
                self.check_expr(rhs, Some(ty));
                return Ty::Error;
            }
            return self.infer_builtin_binary(lhs, op, op_span, ty, rhs);
        }
        match op {
            BinaryOp::And | BinaryOp::Or => {
                self.check_expr(lhs, Some(Ty::Bool));
                self.check_expr(rhs, Some(Ty::Bool));
                Ty::Bool
            }
            BinaryOp::Concat
            | BinaryOp::Add
            | BinaryOp::Sub
            | BinaryOp::Mul
            | BinaryOp::Div
            | BinaryOp::Rem => unreachable!("prelude operators are handled above"),
            BinaryOp::In => {
                // The collection decides what the value must be.
                let collection = self.infer_expr(rhs);
                match self.table.resolve(collection) {
                    Ty::List(element) => {
                        self.check_expr(lhs, Some(*element));
                        self.require(*element, Requirement::Eq, origin, op_span);
                    }
                    Ty::Set(element) => {
                        self.check_expr(lhs, Some(*element));
                    }
                    Ty::Map(map) => {
                        self.check_expr(lhs, Some(map.key));
                    }
                    _ => {
                        self.coerce(rhs, collection, Ty::String);
                        self.check_expr(lhs, Some(Ty::String));
                    }
                }
                Ty::Bool
            }
            BinaryOp::Shl | BinaryOp::Shr => {
                let ty = self.infer_expr(lhs);
                self.require(ty, Requirement::Integer, origin, op_span);
                let amount = self.infer_expr(rhs);
                self.require(amount, Requirement::Integer, origin, self.span(rhs));
                ty
            }
            _ => {
                let ty = self.infer_expr(lhs);
                let ty = self.check_expr(rhs, Some(ty));
                let (requirement, result) = match op {
                    BinaryOp::BitAnd | BinaryOp::BitOr | BinaryOp::BitXor => {
                        (Requirement::Integer, ty)
                    }
                    BinaryOp::Eq | BinaryOp::Ne => (Requirement::Eq, Ty::Bool),
                    BinaryOp::Lt | BinaryOp::Le | BinaryOp::Gt | BinaryOp::Ge => {
                        (Requirement::Ord, Ty::Bool)
                    }
                    _ => unreachable!("handled above"),
                };
                self.require(ty, requirement, origin, op_span);
                result
            }
        }
    }

    /// A binary operator with a prelude trait, on built-in values: `lhs` has type `ty`.
    fn infer_builtin_binary(
        &mut self,
        lhs: ExprId,
        op: BinaryOp,
        op_span: Span,
        ty: Ty,
        rhs: ExprId,
    ) -> Ty {
        if op == BinaryOp::Concat {
            if let Ty::List(_) = self.table.resolve(ty) {
                return self.check_expr(rhs, Some(ty));
            }
            self.coerce(lhs, ty, Ty::String);
            return self.check_expr(rhs, Some(Ty::String));
        }
        let ty = self.check_expr(rhs, Some(ty));
        let requirement = match op {
            BinaryOp::Add | BinaryOp::Sub => Requirement::Additive,
            _ => Requirement::Numeric,
        };
        self.require(ty, requirement, Origin::BinaryOp(op), op_span);
        ty
    }

    fn infer_call(
        &mut self,
        expr: ExprId,
        callee: Callee,
        args: &[CallArg],
        owner: OwnerArgs<'_>,
        fn_args: Option<&Spanned<Vec<Ty>>>,
    ) -> Ty {
        match callee {
            Callee::Error => {
                for arg in args {
                    self.infer_expr(arg.value);
                }
                Ty::Error
            }
            Callee::Builtin(builtin) => self.infer_builtin(expr, builtin, args),
            Callee::Fn(id) => self.infer_fn_call(expr, id, args, owner, fn_args),
        }
    }

    /// Reports a call of associated function `method` of type `ty` as a method.
    fn report_not_method(&mut self, ty: Ty, method: &Spanned<String>) {
        let name = match ty {
            Ty::Adt(adt) => adt.name.to_string(),
            other => other.to_string(),
        };
        self.report(
            Diagnostic::error(
                codes::UNKNOWN_METHOD,
                format!(
                    "`{name}->{}` is an associated function, not a method",
                    method.value
                ),
                method.span,
            )
            .with_help(format!("call it as `[{name}->{} ...]`", method.value)),
        );
    }

    /// For a type that lists trait `requirement` but does not implement it for its type
    /// arguments: which built-in trait it lacks.
    fn unmet_supertrait(&self, ty: Ty, requirement: Requirement) -> Option<String> {
        let module = self.checker.module;
        let (Requirement::Trait(t), Some(id)) = (requirement, adt_of(module, ty)) else {
            return None;
        };
        if !implemented_traits(module, id).contains(&t) {
            return None;
        }
        let missing = implied_bounds(module, Bound::Trait(t))
            .into_iter()
            .find(|&bound| {
                matches!(bound, Bound::Builtin(_))
                    && !satisfies(self.cx(), ty, bound_requirement(bound))
            })?;
        Some(format!(
            "`{ty}` implements `{}` only when it implements {}, which it does not for these type arguments",
            module.traits[t].name.value,
            requirement_name(module, bound_requirement(missing))
        ))
    }

    /// The function `name` of type `ty`: its own, or one of a trait it implements (for a
    /// type parameter, of a trait it is bounded by). Several traits having one is reported.
    fn find_function(&mut self, ty: Ty, name: &Spanned<String>) -> FnLookup {
        let module = self.checker.module;
        let traits: Vec<TraitId> = if let Some(id) = adt_of(module, ty) {
            if let Some(function) = module.types[id].function(&name.value) {
                return FnLookup::Found(function);
            }
            implemented_traits(module, id)
        } else if let Some(head) = TypeHead::of(ty) {
            return module
                .impl_function(head, &name.value)
                .map_or(FnLookup::None, |(_, function)| FnLookup::Found(function));
        } else if let Ty::Param(param) = ty {
            let bounds = self
                .generics
                .get(param.index as usize)
                .map(|p| p.bounds.as_slice())
                .unwrap_or_default();
            let mut traits = Vec::new();
            for bound in bounds {
                for implied in implied_bounds(module, bound.value) {
                    if let Bound::Trait(t) = implied
                        && !traits.contains(&t)
                    {
                        traits.push(t);
                    }
                }
            }
            traits
        } else {
            return FnLookup::None;
        };
        let found: Vec<(TraitId, FnId)> = traits
            .iter()
            .filter_map(|&t| module.traits[t].function(&name.value).map(|f| (t, f)))
            .collect();
        match found.as_slice() {
            [] => FnLookup::None,
            [(_, function)] => FnLookup::Found(*function),
            several => {
                let names: Vec<String> = several
                    .iter()
                    .map(|&(t, _)| format!("`{}`", module.traits[t].name.value))
                    .collect();
                let shown = self.show(ty);
                self.report(Diagnostic::error(
                    codes::UNKNOWN_METHOD,
                    format!(
                        "the method `{}` of {shown} is ambiguous: it is in the traits {}",
                        name.value,
                        names.join(" and ")
                    ),
                    name.span,
                ));
                FnLookup::Ambiguous
            }
        }
    }

    /// Reports a method that values of type `resolved` do not have.
    fn report_unknown_method(&mut self, resolved: Ty, method: &Spanned<String>) {
        let shown = self.show(resolved);
        let help = match resolved {
            Ty::Option(_) => Some(
                "the built-in methods of options are `is_some`, `is_none`, `unwrap`, `unwrap_or`, `take` and `clone`; /std/option adds `map`, `expect` and others",
            ),
            Ty::Box(_) => Some(
                "the built-in methods of boxes are `unbox` and `clone`; the boxed value is `$b->value`",
            ),
            Ty::List(_) => Some(
                "the built-in methods of lists are `push`, `pop`, `insert`, `remove`, `swap`, `get`, `contains`, `len`, `is_empty`, `clear` and `clone`; /std/collections adds `sort`, `map`, `filter` and others",
            ),
            Ty::Map(_) => Some(
                "the built-in methods of maps are `insert`, `remove`, `get`, `contains`, `keys`, `values`, `len`, `is_empty`, `clear` and `clone`; /std/collections adds `get_or` and `entries`",
            ),
            Ty::Set(_) => Some(
                "the built-in methods of sets are `insert`, `remove`, `contains`, `len`, `is_empty`, `clear` and `clone`",
            ),
            _ => None,
        }
        .map(str::to_owned);
        let help = match resolved {
            Ty::Param(param) if method.value == "clone" => Some(bound_hint(
                self.checker.module,
                &param.name,
                Requirement::Clone,
            )),
            Ty::Param(param) => Some(format!(
                "a value of type parameter `{}` has only the methods of the traits it is bound by",
                param.name
            )),
            _ => help,
        };
        let mut diagnostic = Diagnostic::error(
            codes::UNKNOWN_METHOD,
            format!("{shown} has no method named `{}`", method.value),
            method.span,
        );
        if let Some(help) = help {
            diagnostic = diagnostic.with_help(help);
        }
        self.report(diagnostic);
    }

    /// `$out->write value`: appends the text of a value to a formatter.
    fn infer_write(&mut self, expr: ExprId, args: &[CallArg]) -> Ty {
        let span = self.span(expr);
        if let [CallArg { name: None, value }] = args {
            let ty = self.infer_expr(*value);
            let value_span = self.span(*value);
            self.require(ty, Requirement::Display, Origin::Write, value_span);
        } else {
            for arg in args {
                self.infer_expr(arg.value);
            }
            self.report(Diagnostic::error(
                codes::ARGUMENT_MISMATCH,
                "`write` takes one value, as in `$out->write $x`",
                span,
            ));
        }
        self.methods.insert(expr, Method::Write);
        Ty::Nothing
    }

    /// The field named `name` of struct type `ty` that holds a function value, with its
    /// position and type.
    fn function_field(&self, ty: Ty, name: &str) -> Option<(u32, Ty)> {
        let module = self.checker.module;
        let id = struct_of(module, ty)?;
        let (index, field) = module.types[id].field(name)?;
        let field_ty = field.ty.value.subst(&ty.components());
        matches!(field_ty, Ty::Fn(_)).then(|| (u32::try_from(index).expect("few fields"), field_ty))
    }

    /// `[$value->field args]`: a call of the function value in a field.
    fn infer_field_call(
        &mut self,
        expr: ExprId,
        index: u32,
        field_ty: Ty,
        args: &[CallArg],
        span: Span,
    ) -> Ty {
        let Ty::Fn(function) = field_ty else {
            return Ty::Error;
        };
        self.methods.insert(expr, Method::CallField(index));
        if args.len() != function.params.len() {
            self.report(Diagnostic::error(
                codes::ARGUMENT_MISMATCH,
                format!(
                    "this function value takes {} argument{}, but {} were given",
                    function.params.len(),
                    if function.params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
                span,
            ));
        }
        for (position, arg) in args.iter().enumerate() {
            if let Some(name) = &arg.name {
                self.report(Diagnostic::error(
                    codes::ARGUMENT_MISMATCH,
                    "the arguments of a function value are positional",
                    name.span,
                ));
            }
            self.check_expr(arg.value, function.params.get(position).copied());
        }
        self.check_raise_mark(expr, function.raises);
        if function.raises {
            self.check_can_raise(self.span(expr), "an error raised by this function value");
        }
        function.ret
    }

    fn infer_method_call(
        &mut self,
        expr: ExprId,
        receiver: ExprId,
        method: &Spanned<String>,
        args: &[CallArg],
        type_args: Option<&Spanned<Vec<Ty>>>,
    ) -> Ty {
        let receiver_ty = self.infer_expr(receiver);
        let mut resolved = self.table.resolve_deep(receiver_ty);
        if matches!(resolved, Ty::Error) {
            for arg in args {
                self.infer_expr(arg.value);
            }
            return Ty::Error;
        }
        // A number whose type is not known yet has its default type when a method is called
        // on it, as `2.0` in `[2.0->sqrt]` is an `f64`.
        if let Ty::Var(var) = resolved {
            let default = match self.table.kind(var) {
                VarKind::Int => Some(Ty::Int(IntTy::I64)),
                VarKind::Float => Some(Ty::Float(FloatTy::F64)),
                VarKind::General => None,
            };
            if let Some(default) = default {
                let _ = self.table.unify(resolved, default);
                resolved = default;
            }
        }
        let module = self.checker.module;
        let function = match self.find_function(resolved, method) {
            FnLookup::Found(function) => Some(function),
            FnLookup::Ambiguous => {
                for arg in args {
                    self.infer_expr(arg.value);
                }
                return Ty::Error;
            }
            FnLookup::None => None,
        };
        if let Some(function) = function {
            let is_method = module.functions[function]
                .params
                .first()
                .is_some_and(|p| p.is_self);
            let owner = if is_method {
                OwnerArgs::Receiver(resolved)
            } else {
                self.report_not_method(resolved, method);
                OwnerArgs::Written {
                    args: None,
                    self_ty: Some(resolved),
                }
            };
            self.methods.insert(expr, Method::Fn(function));
            return self.infer_fn_call(expr, function, args, owner, type_args);
        }
        if let Some(type_args) = type_args {
            self.report(Diagnostic::error(
                codes::ARGUMENT_MISMATCH,
                format!(
                    "the built-in method `{}` takes no type arguments",
                    method.value
                ),
                type_args.span,
            ));
        }
        if resolved == Ty::Formatter && method.value == "write" {
            return self.infer_write(expr, args);
        }
        if let Some((index, field_ty)) = self.function_field(resolved, &method.value) {
            return self.infer_field_call(expr, index, field_ty, args, method.span);
        }
        let Some(found) = builtin_method(self.cx(), &method.value, resolved) else {
            for arg in args {
                self.infer_expr(arg.value);
            }
            self.report_unknown_method(resolved, method);
            return Ty::Error;
        };
        let names: Vec<String> = found.params.iter().map(|&(n, _)| n.to_owned()).collect();
        let tys: Vec<Ty> = found.params.iter().map(|&(_, t)| t).collect();
        let name = format!("->{}", method.value);
        self.bind_args(expr, &name, &names, &tys, &vec![false; tys.len()], args);
        if let Some((element, requirement)) = found.needs {
            self.require(
                element,
                requirement,
                Origin::Method(found.name),
                method.span,
            );
        }
        self.methods.insert(expr, found.method);
        found.ret
    }

    fn infer_builtin(&mut self, expr: ExprId, builtin: Builtin, args: &[CallArg]) -> Ty {
        let (min, max) = match builtin {
            Builtin::Nothing | Builtin::Default => (0, 0),
            Builtin::Assert => (1, 2),
            _ => (1, 1),
        };
        let mut positional = Vec::new();
        for arg in args {
            if let Some(name) = &arg.name {
                self.report(Diagnostic::error(
                    codes::ARGUMENT_MISMATCH,
                    format!("`:{}` does not take named arguments", builtin.name()),
                    name.span,
                ));
                self.infer_expr(arg.value);
            } else {
                positional.push(arg.value);
            }
        }
        if positional.len() < min || positional.len() > max {
            let expected = if min == max {
                format!("{min} argument{}", if min == 1 { "" } else { "s" })
            } else {
                format!("{min} or {max} arguments")
            };
            self.report(Diagnostic::error(
                codes::ARGUMENT_MISMATCH,
                format!(
                    "`:{}` takes {expected}, but {} were given",
                    builtin.name(),
                    positional.len()
                ),
                self.span(expr),
            ));
        }
        let origin = Origin::Builtin(builtin);
        let mut values = positional.into_iter();
        let mut next = |ctx: &mut Self, expected: Option<Ty>| {
            values.next().map(|v| (v, ctx.check_expr(v, expected)))
        };
        let result = match builtin {
            Builtin::Put => {
                if let Some((value, ty)) = next(self, None) {
                    self.require(ty, Requirement::Display, origin, self.span(value));
                }
                Ty::Nothing
            }
            Builtin::ToStr => {
                if let Some((value, ty)) = next(self, None) {
                    self.require(ty, Requirement::Display, origin, self.span(value));
                }
                Ty::String
            }
            Builtin::Len => {
                if let Some((value, ty)) = next(self, None) {
                    self.require(ty, Requirement::Len, origin, self.span(value));
                }
                Ty::Int(IntTy::I64)
            }
            Builtin::TypeOf => {
                next(self, None);
                Ty::String
            }
            Builtin::Assert => {
                next(self, Some(Ty::Bool));
                next(self, Some(Ty::String));
                Ty::Nothing
            }
            Builtin::Panic => {
                next(self, Some(Ty::String));
                Ty::Never
            }
            Builtin::Nothing => Ty::Nothing,
            Builtin::Default => unreachable!("checked by `infer_default`"),
        };
        for extra in values {
            self.infer_expr(extra);
        }
        result
    }

    /// `[:fn ...]`: a function value. A value this body itself captured is shared by the
    /// copies of its function value, so a new function value captures a clone of it.
    fn infer_closure(&mut self, expr: ExprId, id: FnId) -> Ty {
        let function = &self.checker.module.functions[id];
        let span = self.span(expr);
        for capture in &function.captures {
            if self.body.locals[capture.outer].kind == LocalKind::Captured {
                let ty = self.local_ty(capture.outer);
                self.require(ty, Requirement::Clone, Origin::NestedCapture, span);
            }
        }
        fn_value_ty(function)
    }

    /// `$name` for a function: its value, if it can be one.
    fn infer_fn_ref(&mut self, expr: ExprId, id: FnId) -> Ty {
        let module = self.checker.module;
        let function = &module.functions[id];
        let span = self.span(expr);
        let name = &function.name.value;
        let problem = if !function.generics.is_empty() {
            Some(format!(
                "the generic function `:{name}` cannot be used as a value; wrap a call in a function value: `[:fn x:i64 -> i64 do={{ :return [:{name} $x] }}]`"
            ))
        } else if function
            .params
            .iter()
            .any(|p| p.convention != Convention::Read)
        {
            Some(format!(
                "`:{name}` cannot be used as a value: a function value borrows its arguments for reading, and `:{name}` has `mut` or `owned` parameters"
            ))
        } else {
            None
        };
        if let Some(message) = problem {
            self.report(Diagnostic::error(
                codes::NOT_A_FUNCTION_VALUE,
                message,
                span,
            ));
            return Ty::Error;
        }
        fn_value_ty(function)
    }

    /// `[$f args]`: a call of a function value.
    fn infer_value_call(&mut self, expr: ExprId, callee: ExprId, args: &[ExprId]) -> Ty {
        let callee_ty = self.infer_expr(callee);
        let resolved = self.table.resolve_deep(callee_ty);
        let Ty::Fn(function) = resolved else {
            for &arg in args {
                self.infer_expr(arg);
            }
            if resolved != Ty::Error {
                let shown = self.show(resolved);
                self.report(Diagnostic::error(
                    codes::NOT_A_FUNCTION_VALUE,
                    format!("a value of type {shown} cannot be called"),
                    self.span(callee),
                ));
            }
            return Ty::Error;
        };
        if args.len() != function.params.len() {
            self.report(Diagnostic::error(
                codes::ARGUMENT_MISMATCH,
                format!(
                    "this function value takes {} argument{}, but {} were given",
                    function.params.len(),
                    if function.params.len() == 1 { "" } else { "s" },
                    args.len()
                ),
                self.span(expr),
            ));
        }
        for (index, &arg) in args.iter().enumerate() {
            let expected = function.params.get(index).copied();
            self.check_expr(arg, expected);
        }
        self.check_raise_mark(expr, function.raises);
        if function.raises {
            let span = self.span(expr);
            self.check_can_raise(span, "an error raised by this function value");
        }
        function.ret
    }

    /// `:onerror e in={...} do={...}`: errors raised in the body are caught. It never finishes
    /// normally if neither block does.
    fn check_onerror(&mut self, error: LocalId, body: &Block, handler: &Block) -> bool {
        self.catching += 1;
        let body_diverges = self.check_block(body);
        self.catching -= 1;
        let error_ty = self.checker.error_ty();
        self.local_tys.insert(error, error_ty);
        let handler_diverges = self.check_block(handler);
        body_diverges && handler_diverges
    }

    /// Reports raising an error, by `what`, where nothing catches or propagates it.
    /// Checks that the call `expr`, which raises errors if `raises`, has a `?` after its head
    /// exactly then (spec section 9.1).
    fn check_raise_mark(&mut self, expr: ExprId, raises: bool) {
        let Some(head) = self.body.call_heads.get(expr) else {
            return;
        };
        self.marks_checked.insert(expr, ());
        match (raises, head.mark) {
            (true, None) => self.report(
                Diagnostic::error(
                    codes::MISSING_RAISE_MARK,
                    format!(
                        "`{}` can raise an error, so its call needs a `?`",
                        head.text
                    ),
                    head.span,
                )
                .with_help(format!(
                    "write `{}?` to pass the error on, or catch it with `:onerror`",
                    head.text
                )),
            ),
            (false, Some(mark)) => self.report_needless_mark(&head.text, mark),
            _ => {}
        }
    }

    fn report_needless_mark(&mut self, head: &str, mark: Span) {
        self.report(
            Diagnostic::error(
                codes::NEEDLESS_RAISE_MARK,
                format!("`{head}` cannot raise an error, so its call takes no `?`"),
                mark,
            )
            .with_help("remove the `?`"),
        );
    }

    fn check_can_raise(&mut self, span: Span, what: &str) {
        if !self.in_function || self.raises || self.catching > 0 {
            return;
        }
        self.report(
            Diagnostic::error(codes::UNCAUGHT_ERROR, format!("{what} is not caught"), span)
                .with_help(
                    "catch it with `:onerror e in={ ... } do={ ... }`, or declare the function `raises` to pass it on",
                ),
        );
    }

    /// `:error value` or `:error value source=$cause`: the value is a message or an `Error`.
    fn infer_raise(&mut self, expr: ExprId, value: ExprId, source: Option<ExprId>) -> Ty {
        let span = self.span(expr);
        self.check_can_raise(span, "the error raised here");
        let error_ty = self.checker.error_ty();
        let ty = self.infer_expr(value);
        let resolved = self.table.resolve(ty);
        let is_error = resolved == error_ty;
        if !is_error && self.table.unify(ty, Ty::String).is_err() {
            let shown = self.show(ty);
            self.report(Diagnostic::error(
                codes::MISMATCHED_TYPES,
                format!("`:error` takes a message `String` or an `Error`, not {shown}"),
                self.span(value),
            ));
        }
        if let Some(source) = source {
            self.check_expr(source, Some(error_ty));
            if is_error {
                self.report(
                    Diagnostic::error(
                        codes::ARGUMENT_MISMATCH,
                        "`source=` gives the cause of a new error, made from a message",
                        self.span(source),
                    )
                    .with_help("raise the error as it is, or write `:error \"message\" source=$e`"),
                );
            }
        }
        Ty::Never
    }

    /// `[:default]` or `[:default<T>]`: the default value of the written or expected type.
    fn infer_default(
        &mut self,
        expr: ExprId,
        args: &[CallArg],
        fn_args: Option<&Spanned<Vec<Ty>>>,
        expected: Option<Ty>,
    ) -> Ty {
        let span = self.span(expr);
        for arg in args {
            self.infer_expr(arg.value);
        }
        if !args.is_empty() {
            self.report(Diagnostic::error(
                codes::ARGUMENT_MISMATCH,
                "`:default` takes no arguments",
                span,
            ));
        }
        let ty = match (fn_args, expected) {
            (Some(written), _) => {
                let &[ty] = written.value.as_slice() else {
                    self.report(Diagnostic::error(
                        codes::ARGUMENT_MISMATCH,
                        "`:default` takes one type argument, as in `[:default<i64>]`",
                        written.span,
                    ));
                    return Ty::Error;
                };
                ty
            }
            (None, Some(expected)) => expected,
            (None, None) => {
                self.report(
                    Diagnostic::error(
                        codes::CANNOT_INFER,
                        "cannot infer the type of `[:default]`",
                        span,
                    )
                    .with_help("write the type: `[:default<Point>]`"),
                );
                return Ty::Error;
            }
        };
        self.require(
            ty,
            Requirement::Default,
            Origin::Builtin(Builtin::Default),
            span,
        );
        ty
    }

    /// Checks a call of a user-defined function: its type arguments, then its arguments. For
    /// a method call, `receiver` is the type of the receiver, which binds `self` and the
    /// type arguments of the method's type; the arguments bind to the parameters after
    /// `self`.
    fn infer_fn_call(
        &mut self,
        expr: ExprId,
        id: FnId,
        args: &[CallArg],
        owner: OwnerArgs<'_>,
        fn_args: Option<&Spanned<Vec<Ty>>>,
    ) -> Ty {
        let module = self.checker.module;
        let function = &module.functions[id];
        let fn_name = &callable_name(module, id);
        let generics = &function.generics.params;
        let split = function.own_generics;
        let span = self.span(expr);
        let mut instance = self.owner_instance(function, owner, span);
        self.check_raise_mark(expr, function.raises);
        if function.raises {
            self.check_can_raise(span, &format!("an error raised by `{fn_name}`"));
        }
        if is_drop_function(module, id) {
            self.report(
                Diagnostic::error(
                    codes::UNKNOWN_METHOD,
                    "`drop` runs when a value is destroyed; it cannot be called",
                    span,
                )
                .with_help(
                    "a value is destroyed when its variable goes out of scope, or when `:set` replaces it",
                ),
            );
        }
        if let Some(owner) = function.owner {
            let owner_name = match owner {
                FnOwner::Type(ty) => module.types[ty].name.value.clone(),
                FnOwner::Trait(t) => module.traits[t].name.value.clone(),
                FnOwner::Impl(i) => module.impls[i].self_ty.value.to_string(),
            };
            let what = if function.params.first().is_some_and(|p| p.is_self) {
                "method"
            } else {
                "function"
            };
            self.check_visible(
                function.module,
                &owner_name,
                &function.name.value,
                what,
                span,
            );
        }
        let own = generics.len() - split;
        match fn_args {
            Some(written) if written.value.len() == own => instance.extend(&written.value),
            Some(written) => {
                let message = if own == 0 {
                    format!("`{fn_name}` does not take type arguments")
                } else {
                    format!(
                        "`{fn_name}` takes {own} type argument{}, but {} were given",
                        if own == 1 { "" } else { "s" },
                        written.value.len()
                    )
                };
                self.report(Diagnostic::error(
                    codes::ARGUMENT_MISMATCH,
                    message,
                    written.span,
                ));
                instance.extend((0..own).map(|_| Ty::Error));
            }
            None => instance.extend((0..own).map(|_| self.table.new_var(VarKind::General))),
        }
        self.require_bounds(generics, &instance, BoundItem::Fn(id), span);
        let skip = usize::from(matches!(owner, OwnerArgs::Receiver(_)));
        let (param_tys, param_names, has_default, ret) = {
            let sig = &self.checker.sigs[id];
            let params = sig.params.get(skip..).unwrap_or_default();
            (
                params
                    .iter()
                    .map(|p| p.ty.subst(&instance))
                    .collect::<Vec<_>>(),
                params.iter().map(|p| p.name.clone()).collect::<Vec<_>>(),
                params.iter().map(|p| p.has_default).collect::<Vec<_>>(),
                sig.ret.subst(&instance),
            )
        };
        self.bind_args(expr, fn_name, &param_names, &param_tys, &has_default, args);
        if !instance.is_empty() {
            self.instances.insert(expr, instance);
        }
        ret
    }

    /// The type arguments of the owner of `function`: those of its type, or the `Self` type
    /// of a trait's function.
    fn owner_instance(&mut self, function: &Function, owner: OwnerArgs<'_>, span: Span) -> Vec<Ty> {
        let split = function.own_generics;
        let fresh = |table: &mut Table, count: usize| -> Vec<Ty> {
            (0..count)
                .map(|_| table.new_var(VarKind::General))
                .collect()
        };
        match (function.owner, owner) {
            (Some(FnOwner::Impl(id)), owner) => {
                // The impl's type parameters are what makes its type the type the function is
                // called on.
                let args = fresh(&mut self.table, split);
                let declared = self.checker.module.impls[id].self_ty.value;
                let actual = match owner {
                    OwnerArgs::Receiver(ty) => Some(ty),
                    OwnerArgs::Written { self_ty, .. } => self_ty,
                };
                if let Some(actual) = actual
                    && self.table.unify(declared.subst(&args), actual).is_err()
                {
                    let shown = self.show(actual);
                    self.report(Diagnostic::error(
                        codes::MISMATCHED_TYPES,
                        format!(
                            "`{}` is a function of `{declared}`, not of {shown}",
                            function.name.value
                        ),
                        span,
                    ));
                }
                args
            }
            (Some(FnOwner::Trait(_)), OwnerArgs::Receiver(ty)) => vec![ty],
            (Some(FnOwner::Trait(_)), OwnerArgs::Written { args, self_ty }) => {
                let self_ty = match self_ty {
                    Some(ty @ Ty::Adt(adt)) => {
                        let count = adt.args.len();
                        let args = match args {
                            Some(written) if written.value.len() == count => written.value.clone(),
                            _ => fresh(&mut self.table, count),
                        };
                        ty.subst(&args)
                    }
                    Some(ty) => ty,
                    None => self.table.new_var(VarKind::General),
                };
                vec![self_ty]
            }
            (_, OwnerArgs::Receiver(Ty::Adt(adt))) if adt.args.len() == split => adt.args.to_vec(),
            (
                _,
                OwnerArgs::Written {
                    args: Some(written),
                    ..
                },
            ) if written.value.len() == split => written.value.clone(),
            _ => fresh(&mut self.table, split),
        }
    }

    /// Matches the positional and named arguments of a call to parameters, checks each
    /// against its parameter's type, and records the value of each parameter in
    /// `call_args`.
    fn bind_args(
        &mut self,
        expr: ExprId,
        fn_name: &str,
        param_names: &[String],
        param_tys: &[Ty],
        has_default: &[bool],
        args: &[CallArg],
    ) {
        let positional_count = args.iter().filter(|a| a.name.is_none()).count();
        let mut bound: Vec<Option<ExprId>> = vec![None; param_tys.len()];
        let mut next_positional = 0;
        let mut seen_named = false;
        let mut reported_too_many = false;
        for arg in args {
            let target = match &arg.name {
                None => {
                    if seen_named {
                        self.report(Diagnostic::error(
                            codes::ARGUMENT_MISMATCH,
                            "positional arguments must come before named arguments",
                            self.span(arg.value),
                        ));
                    }
                    let index = next_positional;
                    next_positional += 1;
                    if index >= param_tys.len() {
                        if !reported_too_many {
                            reported_too_many = true;
                            self.report(Diagnostic::error(
                                codes::ARGUMENT_MISMATCH,
                                format!(
                                    "`{fn_name}` takes {} argument{}, but {positional_count} were given",
                                    param_tys.len(),
                                    if param_tys.len() == 1 { "" } else { "s" }
                                ),
                                self.span(arg.value),
                            ));
                        }
                        None
                    } else {
                        Some(index)
                    }
                }
                Some(name) => {
                    seen_named = true;
                    self.named_param_index(fn_name, param_names, name)
                }
            };
            match target {
                Some(index) if bound[index].is_some() => {
                    self.report(Diagnostic::error(
                        codes::ARGUMENT_MISMATCH,
                        format!("argument `{}` is given more than once", param_names[index]),
                        self.span(arg.value),
                    ));
                    self.infer_expr(arg.value);
                }
                Some(index) => {
                    bound[index] = Some(arg.value);
                    self.check_expr(arg.value, Some(param_tys[index]));
                }
                None => {
                    self.infer_expr(arg.value);
                }
            }
        }

        self.report_missing_args(expr, fn_name, &bound, param_names, has_default);
        self.call_args.insert(
            expr,
            bound
                .iter()
                .map(|value| value.map_or(ArgValue::Default, ArgValue::Expr))
                .collect(),
        );
    }

    /// The index of the parameter called `name`, reporting an unknown name.
    fn named_param_index(
        &mut self,
        fn_name: &str,
        params: &[String],
        name: &Spanned<String>,
    ) -> Option<usize> {
        let index = params.iter().position(|p| *p == name.value);
        if index.is_none() {
            let names: Vec<String> = params.iter().map(|n| format!("`{n}`")).collect();
            let help = if names.is_empty() {
                format!("`{fn_name}` has no parameters")
            } else {
                format!("the parameters of `{fn_name}` are {}", names.join(", "))
            };
            self.report(
                Diagnostic::error(
                    codes::ARGUMENT_MISMATCH,
                    format!("`{fn_name}` has no parameter named `{}`", name.value),
                    name.span,
                )
                .with_help(help),
            );
        }
        index
    }

    fn report_missing_args(
        &mut self,
        expr: ExprId,
        fn_name: &str,
        bound: &[Option<ExprId>],
        params: &[String],
        has_default: &[bool],
    ) {
        let missing: Vec<String> = bound
            .iter()
            .enumerate()
            .filter(|&(index, value)| value.is_none() && !has_default[index])
            .map(|(index, _)| format!("`{}`", params[index]))
            .collect();
        if missing.is_empty() {
            return;
        }
        self.report(Diagnostic::error(
            codes::ARGUMENT_MISMATCH,
            format!(
                "missing argument{} {} in call to `{fn_name}`",
                if missing.len() == 1 { "" } else { "s" },
                missing.join(", ")
            ),
            self.span(expr),
        ));
    }

    // ----- Statements ----------------------------------------------------------------------

    /// Checks a block and returns true if it never finishes normally.
    fn check_block(&mut self, block: &Block) -> bool {
        let mut diverges = false;
        let mut warned = false;
        for &stmt in &block.stmts {
            if diverges && !warned {
                warned = true;
                let span = self.body.stmt_span(stmt);
                self.report(
                    Diagnostic::warning(codes::UNREACHABLE_CODE, "unreachable code", span)
                        .with_help("the statement before this one never finishes"),
                );
            }
            diverges |= self.check_stmt(stmt);
        }
        diverges
    }

    /// Checks a statement and returns true if it never finishes normally.
    fn check_stmt(&mut self, stmt: StmtId) -> bool {
        let body = self.body;
        match &body.stmts[stmt] {
            Stmt::Expr(expr) => self.infer_expr(*expr) == Ty::Never,
            Stmt::Let { local, init } => {
                let declared = match &body.locals[*local].ty {
                    Some(ty) => {
                        self.checker.check_type_wf(ty.value, ty.span, self.generics);
                        ty.value
                    }
                    None => self.table.new_var(VarKind::General),
                };
                let diverges =
                    init.is_some_and(|init| self.check_expr(init, Some(declared)) == Ty::Never);
                self.local_tys.insert(*local, declared);
                diverges
            }
            Stmt::Set { target, value } => {
                let expected = match *target {
                    Place::Local(local) => Some(self.local_ty(local)),
                    Place::Global(global) => self.checker.result.globals.get(global).copied(),
                    Place::Part(part) => Some(self.infer_expr(part)),
                    Place::Error => None,
                };
                self.check_expr(*value, expected) == Ty::Never
            }
            Stmt::If {
                cond,
                then_block,
                else_branch,
            } => {
                let cond_diverges = self.check_expr(*cond, Some(Ty::Bool)) == Ty::Never;
                let then_diverges = self.check_block(then_block);
                let else_diverges = match else_branch {
                    Some(ElseBranch::Block(block)) => self.check_block(block),
                    Some(ElseBranch::If(nested)) => self.check_stmt(*nested),
                    None => false,
                };
                cond_diverges || (then_diverges && else_diverges)
            }
            Stmt::While {
                cond,
                body: loop_body,
            } => {
                let cond_diverges = self.check_expr(*cond, Some(Ty::Bool)) == Ty::Never;
                let always = matches!(body.exprs[*cond], Expr::Literal(Literal::Bool(true)));
                let has_break = self.check_loop_body(loop_body).1;
                cond_diverges || (always && !has_break)
            }
            Stmt::DoWhile {
                body: loop_body,
                cond,
            } => {
                let (body_diverges, has_break) = self.check_loop_body(loop_body);
                self.check_expr(*cond, Some(Ty::Bool));
                let always = matches!(body.exprs[*cond], Expr::Literal(Literal::Bool(true)));
                (body_diverges || always) && !has_break
            }
            Stmt::For {
                var,
                from,
                end,
                step,
                body: loop_body,
            } => {
                let ty = self.infer_expr(*from);
                self.require(ty, Requirement::Integer, Origin::ForLoop, self.span(*from));
                if let Some((_, end)) = end {
                    self.check_expr(*end, Some(ty));
                }
                if let Some(step) = step {
                    self.check_expr(*step, Some(ty));
                }
                self.local_tys.insert(*var, ty);
                self.check_loop_body(loop_body);
                false
            }
            Stmt::Block(block) => self.check_block(block),
            Stmt::Match { scrutinee, arms } => self.check_match(*scrutinee, arms),
            Stmt::OnError {
                error,
                body,
                handler,
            } => self.check_onerror(*error, body, handler),
            &Stmt::Foreach {
                key,
                value,
                collection,
                ref body,
            } => {
                self.check_foreach(key, value, collection);
                self.check_loop_body(body);
                false
            }
        }
    }

    /// Gives the variables of a `:foreach` their types from the collection.
    fn check_foreach(&mut self, key: Option<LocalId>, value: LocalId, collection: ExprId) {
        let ty = self.infer_expr(collection);
        let resolved = self.table.resolve(ty);
        let mutable = matches!(
            self.body.locals[value].kind,
            pika_hir::LocalKind::Element { mutable: true }
        );
        let (key_ty, value_ty) = match resolved {
            Ty::List(element) => (Some(Ty::Int(IntTy::I64)), *element),
            Ty::Map(map) => (Some(map.key), map.value),
            // A set has no index; its elements are keys, which cannot change.
            Ty::Set(element) => {
                if key.is_some() || mutable {
                    let message = if mutable {
                        "the elements of a set cannot be modified"
                    } else {
                        "a set has no index; iterate with one name: `:foreach x in=$set`"
                    };
                    self.report(Diagnostic::error(
                        codes::NOT_A_COLLECTION,
                        message,
                        self.span(collection),
                    ));
                }
                (Some(Ty::Error), *element)
            }
            Ty::Error => (Some(Ty::Error), Ty::Error),
            other => {
                let shown = self.show(other);
                let mut diagnostic = Diagnostic::error(
                    codes::NOT_A_COLLECTION,
                    format!("{shown} cannot be iterated with `:foreach`"),
                    self.span(collection),
                );
                diagnostic = match other {
                    Ty::Int(_) => diagnostic.with_help("count with `:for i from=0 until=$n`"),
                    Ty::Var(_) => diagnostic.with_help("add a type annotation to the collection"),
                    _ => diagnostic.with_help("`:foreach` iterates over lists, maps and sets"),
                };
                self.report(diagnostic);
                (Some(Ty::Error), Ty::Error)
            }
        };
        if let Some(key) = key {
            self.local_tys.insert(key, key_ty.unwrap_or(Ty::Error));
        }
        self.local_tys.insert(value, value_ty);
    }

    /// Checks a `:match` and returns true if it never finishes normally: every arm diverges.
    fn check_match(&mut self, scrutinee: ExprId, arms: &[MatchArm]) -> bool {
        let ty = self.infer_expr(scrutinee);
        let mut all_diverge = !arms.is_empty();
        let mut invalid = false;
        for arm in arms {
            invalid |= !self.check_pattern(arm.pat, ty);
            if let Some(guard) = arm.guard {
                self.check_expr(guard, Some(Ty::Bool));
            }
            all_diverge &= self.check_block(&arm.body);
        }
        self.matches.push(PendingMatch {
            scrutinee,
            arms: arms.iter().map(|a| (a.pat, a.guard.is_some())).collect(),
            invalid,
        });
        all_diverge
    }

    /// Checks that a pattern can match values of type `ty`, and gives its bindings their
    /// types. Returns false if the pattern is invalid (already reported).
    fn check_pattern(&mut self, pat: PatId, ty: Ty) -> bool {
        let body = self.body;
        let span = body.pat_span(pat);
        self.pat_tys.insert(pat, ty);
        let pattern_ty = match &body.pats[pat] {
            Pat::Invalid(parts) => {
                for &part in parts {
                    self.check_pattern(part, Ty::Error);
                }
                return false;
            }
            Pat::Wildcard => return true,
            &Pat::Binding(local) => {
                self.local_tys.insert(local, ty);
                return true;
            }
            Pat::Literal(literal) => match *literal {
                Literal::Int { value, negative } => {
                    self.pat_int_literals.push((pat, value, negative));
                    self.table.new_var(VarKind::Int)
                }
                Literal::Bool(_) => Ty::Bool,
                Literal::Char(_) => Ty::Char,
                Literal::Float(_) | Literal::Duration(_) => return false,
            },
            Pat::String(_) => Ty::String,
            Pat::None => Ty::option(self.table.new_var(VarKind::General)),
            &Pat::Some(inner) => {
                let inner_ty = self.table.new_var(VarKind::General);
                let matches = self.unify_pattern(span, Ty::option(inner_ty), ty);
                let inner_ty = if matches { inner_ty } else { Ty::Error };
                return self.check_pattern(inner, inner_ty) && matches;
            }
            Pat::Variant {
                ty: enum_ty,
                variant,
                fields,
                type_args,
            } => {
                let def = &self.checker.module.types[*enum_ty];
                let args = self.instantiate(
                    &def.generics.params,
                    type_args.as_ref(),
                    BoundItem::Type(*enum_ty),
                    span,
                    Some(ty),
                );
                let matches = self.unify_pattern(span, def.ty.subst(&args), ty);
                let mut valid = matches;
                for (field_pat, field) in fields.iter().zip(&def.variants[*variant].fields) {
                    let field_ty = if matches {
                        field.ty.value.subst(&args)
                    } else {
                        Ty::Error
                    };
                    valid &= self.check_pattern(*field_pat, field_ty);
                }
                return valid;
            }
        };
        self.unify_pattern(span, pattern_ty, ty)
    }

    /// Unifies the type of a pattern with the type of the value matched, reporting a
    /// mismatch.
    fn unify_pattern(&mut self, span: Span, pattern_ty: Ty, value_ty: Ty) -> bool {
        if self.table.unify(pattern_ty, value_ty).is_ok() {
            return true;
        }
        let shown_pattern = match self.table.resolve(pattern_ty) {
            Ty::Option(inner) if matches!(self.table.resolve(*inner), Ty::Var(_)) => {
                "an option".to_owned()
            }
            _ => self.show(pattern_ty),
        };
        let shown_value = self.show(value_ty);
        self.report(Diagnostic::error(
            codes::MISMATCHED_TYPES,
            format!("this pattern matches {shown_pattern}, but the value is {shown_value}"),
            span,
        ));
        false
    }

    /// Checks a loop body; returns whether it diverges and whether it contains `:break`.
    fn check_loop_body(&mut self, block: &Block) -> (bool, bool) {
        self.loops.push(LoopFrame { has_break: false });
        let diverges = self.check_block(block);
        let frame = self.loops.pop().expect("pushed above");
        (diverges, frame.has_break)
    }

    // ----- Finishing -----------------------------------------------------------------------

    /// Applies literal defaults, checks deferred requirements and literal ranges, and
    /// returns the final types.
    fn finish(mut self) -> BodyTypes {
        self.table.apply_defaults();

        // A `?` on a call of something else than a function that raises: a built-in command or
        // method, a variant or another value. Calls whose callee is in error were reported.
        let unchecked: Vec<(String, Span)> = self
            .body
            .call_heads
            .iter()
            .filter(|&(expr, head)| {
                head.mark.is_some()
                    && self.marks_checked.get(expr).is_none()
                    && self
                        .expr_tys
                        .get(expr)
                        .is_none_or(|&ty| self.table.resolve(ty) != Ty::Error)
            })
            .filter_map(|(_, head)| Some((head.text.clone(), head.mark?)))
            .collect();
        for (head, mark) in unchecked {
            self.report_needless_mark(&head, mark);
        }

        for pending in std::mem::take(&mut self.matches) {
            self.check_exhaustive(&pending);
        }

        for obligation in std::mem::take(&mut self.obligations) {
            let ty = self.table.resolve_deep(obligation.ty);
            if !ty.has_vars() && !satisfies(self.cx(), ty, obligation.requirement) {
                self.report_requirement(
                    ty,
                    obligation.requirement,
                    obligation.origin,
                    obligation.span,
                );
            }
        }

        self.check_literal_ranges();

        let mut types = BodyTypes {
            call_args: std::mem::take(&mut self.call_args),
            methods: std::mem::take(&mut self.methods),
            projections: std::mem::take(&mut self.projections),
            ..BodyTypes::default()
        };
        let mut reported = Vec::new();
        for (local, &ty) in self.local_tys.iter() {
            let resolved = self.table.resolve_deep(ty);
            if resolved.has_vars() {
                if !reported.contains(&resolved) {
                    reported.push(resolved);
                    let name = &self.body.locals[local].name;
                    self.checker.report(
                        Diagnostic::error(
                            codes::CANNOT_INFER,
                            format!("cannot infer the type of `{}`", name.value),
                            name.span,
                        )
                        .with_help(format!(
                            "add a type annotation, as in `:local {}:i64`",
                            name.value
                        )),
                    );
                }
                types.locals.insert(local, Ty::Error);
            } else {
                types.locals.insert(local, resolved);
            }
        }
        for (expr, &ty) in self.expr_tys.iter() {
            types.exprs.insert(expr, self.final_ty(ty));
        }
        for (pat, &ty) in self.pat_tys.iter() {
            types.pats.insert(pat, self.final_ty(ty));
        }
        for (expr, &ty) in self.wrapped.iter() {
            types.wrapped.insert(expr, self.final_ty(ty));
        }
        for (expr, instance) in self.instances.iter() {
            let instance = instance.iter().map(|&ty| self.final_ty(ty)).collect();
            types.instances.insert(expr, instance);
        }
        types
    }

    /// Reports integer and float literals, and integer patterns, that do not fit their
    /// inferred types.
    fn check_literal_ranges(&mut self) {
        for (pat, value, negative) in std::mem::take(&mut self.pat_int_literals) {
            let ty = self.pat_tys.get(pat).map(|&t| self.table.resolve(t));
            if let Some(Ty::Int(int)) = ty
                && value > int.max_magnitude(negative)
            {
                let sign = if negative { "-" } else { "" };
                self.report(Diagnostic::error(
                    codes::LITERAL_OUT_OF_RANGE,
                    format!(
                        "the pattern `{sign}{value}` does not fit in `{}`",
                        int.name()
                    ),
                    self.body.pat_span(pat),
                ));
            }
        }
        for literal in std::mem::take(&mut self.int_literals) {
            let ty = self
                .expr_tys
                .get(literal.expr)
                .map(|&t| self.table.resolve(t));
            let Some(Ty::Int(int)) = ty else { continue };
            if literal.value > int.max_magnitude(literal.negative) {
                let sign = if literal.negative { "-" } else { "" };
                let min = if int.is_signed() {
                    format!("-{}", int.max_magnitude(true))
                } else {
                    "0".to_owned()
                };
                self.report(
                    Diagnostic::error(
                        codes::LITERAL_OUT_OF_RANGE,
                        format!(
                            "the literal `{sign}{}` does not fit in `{}`",
                            literal.value,
                            int.name()
                        ),
                        self.span(literal.expr),
                    )
                    .with_help(format!(
                        "the range of `{}` is {min} to {}",
                        int.name(),
                        int.max_magnitude(false)
                    )),
                );
            }
        }
        for (expr, value) in std::mem::take(&mut self.float_literals) {
            let ty = self.expr_tys.get(expr).map(|&t| self.table.resolve(t));
            #[allow(
                clippy::cast_possible_truncation,
                reason = "checking whether the value fits"
            )]
            let overflows = ty == Some(Ty::Float(FloatTy::F32)) && (value as f32).is_infinite();
            if overflows {
                self.report(Diagnostic::error(
                    codes::LITERAL_OUT_OF_RANGE,
                    "the literal does not fit in `f32`",
                    self.span(expr),
                ));
            }
        }
    }

    /// A type after inference: variables left unresolved (already reported) become errors.
    fn final_ty(&self, ty: Ty) -> Ty {
        let resolved = self.table.resolve_deep(ty);
        if resolved.has_vars() {
            Ty::Error
        } else {
            resolved
        }
    }

    /// Reports a `:match` that does not cover every value, and arms that cannot match.
    fn check_exhaustive(&mut self, pending: &PendingMatch) {
        let ty = self.final_ty(
            self.expr_tys
                .get(pending.scrutinee)
                .copied()
                .unwrap_or(Ty::Error),
        );
        if ty == Ty::Error || pending.invalid {
            return;
        }
        let check = exhaustive::check_match(self.checker.module, self.body, ty, &pending.arms);
        for index in check.unreachable {
            let pat = pending.arms[index].0;
            self.report(
                Diagnostic::warning(
                    codes::UNREACHABLE_PATTERN,
                    "unreachable pattern",
                    self.body.pat_span(pat),
                )
                .with_help("the arms before this one match every value it matches"),
            );
        }
        if let Some(missing) = check.missing {
            let shown = self.show(ty);
            self.report(
                Diagnostic::error(
                    codes::NON_EXHAUSTIVE_MATCH,
                    format!("no arm of this `:match` matches `{missing}`"),
                    self.span(pending.scrutinee),
                )
                .with_label(format!("a value of type {shown}, which can be `{missing}`"))
                .with_help(format!(
                    "add an arm for `{missing}`, or `_ do={{...}}` for every other value"
                )),
            );
        }
    }
}
