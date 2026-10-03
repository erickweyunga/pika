//! Building MIR from the HIR and the inferred types.
//!
//! Besides flattening expressions into a control-flow graph, the builder applies the ownership
//! rules of spec section 12: it decides where values are copied, moved or borrowed, reports
//! moves out of borrowed places, checks `mut` arguments, and places a drop for every owned
//! value at the end of its scope (and before `:return`, `:break` and `:continue`). Drops of
//! values that may already be moved are resolved later by [`crate::analyze`].

use std::collections::HashMap;

use la_arena::{Arena, ArenaMap};
use pika_diagnostics::{Diagnostic, SourceMap, Span};
use pika_hir::{self as hir, Builtin, Callee, Convention, ElseBranch, ForEnd, IntTy, StringPart};
use pika_types::{ArgValue, BodyTypes, Method, Projection, Ty, TypeckResult};

/// The type of lengths and indexes.
const INT: Ty = Ty::Int(IntTy::I64);

use pika_runtime::intrinsics::Kind;

use crate::consts::ConstValues;
use crate::{
    BasicBlock, BinaryOp, BlockId, Body, CallArg, CallTarget, CastKind, ErrorTarget, InstanceId,
    Intrinsic, LocalDecl, LocalId, LocalMode, Operand, PanicKind, Place, PlaceRoot, PrintPart,
    Program, Rvalue, Statement, Stream, Terminator, Types, UnaryOp, Unsupported, UserVariable,
    Value, codes, unsigned_of,
};

/// Builds the MIR of a module that type checked without errors, and checks its ownership.
///
/// Every function without type parameters is built, and every instance of a generic function
/// it calls, transitively. A generic function is also built once on its own, with its type
/// parameters as types, to check its ownership for all type arguments: its instances do not
/// report the same problems again.
///
/// Constants, global initializers and default parameter values are evaluated at compile time.
/// Failures to evaluate them, and violations of the ownership rules, are returned as
/// diagnostics.
///
/// `sources` gives the file, line and column of the source locations that errors record.
pub fn build_program<'a>(
    module: &'a hir::Module,
    types: &'a TypeckResult,
    sources: &'a SourceMap,
) -> (Program, Vec<Diagnostic>) {
    let structs = Types::from_module(module);
    let mut consts = ConstValues::new(module, types, &structs);
    let mut program = Program::default();
    for (id, _) in module.consts.iter() {
        consts.const_value(id);
    }
    for (id, global) in module.globals.iter() {
        let value = consts.global_value(id);
        program.globals.insert(
            id,
            crate::GlobalInit {
                name: global.name.value.clone(),
                ty: types.globals.get(id).copied().unwrap_or(Ty::Error),
                value,
            },
        );
    }
    let mut diagnostics = check_intrinsics(module);
    let mut instances = Instances::default();
    // The functions of the package being compiled are all built, for their diagnostics; those
    // of its dependencies only when they are used.
    for (id, function) in module.functions.iter() {
        if types.functions.get(id).is_none() || !module.is_local(id) {
            continue;
        }
        if let hir::FnKind::Closure(_) = function.kind
            && function.generics.is_empty()
        {
            // Built when created.
            continue;
        }
        if function.generics.is_empty() {
            instances.request(id, Box::new([]), function.name.span);
        } else {
            diagnostics.extend(check_generic_body(module, &structs, types, &mut consts, id));
        }
    }
    program.entry = module
        .entry
        .map(|id| instances.request(id, Box::new([]), Span::default()));
    // The tests of the package compiled, which `pika test` runs one at a time as the entry.
    program.tests = module
        .tests
        .iter()
        .filter(|test| module.is_local(test.function))
        .map(|test| instances.request(test.function, Box::new([]), test.name.span))
        .collect();
    // Every value lives in a local or a global, so the `drop` functions of their types, and
    // of the types inside them, are all that destroying values can run.
    let global_tys: Vec<Ty> = program.globals.values().map(|global| global.ty).collect();
    for ty in global_tys {
        request_impls(&structs, &mut instances, &mut program, ty);
    }
    let mut unsupported = Vec::new();
    let mut next = 0;
    while let Some((id, args, adapter)) = instances.queue.get(next).cloned() {
        next += 1;
        if adapter {
            let mut body = build_adapter(module, &structs, &mut instances, id);
            diagnostics.extend(crate::analyze(&mut body));
            let alloc = program.functions.alloc(body);
            debug_assert_eq!(alloc.into_raw().into_u32() as usize, next - 1);
            continue;
        }
        let generic = !args.is_empty();
        let instance = Instance {
            subst: args,
            copy_params: Vec::new(),
            locate: Some(sources),
        };
        let (mut body, found, errors) = build_instance(
            module,
            &structs,
            types,
            &mut consts,
            &mut instances,
            id,
            instance,
        );
        let errors_after = crate::analyze(&mut body);
        for (_, local) in body.locals.iter() {
            request_impls(&structs, &mut instances, &mut program, local.ty);
        }
        // The check body of a generic function reported its problems.
        if !generic {
            diagnostics.extend(errors);
            diagnostics.extend(errors_after);
        }
        for feature in found {
            if !unsupported.contains(&feature) {
                unsupported.push(feature);
            }
        }
        let alloc = program.functions.alloc(body);
        debug_assert_eq!(alloc.into_raw().into_u32() as usize, next - 1);
    }
    program.unsupported = unsupported;
    diagnostics.extend(instances.errors);
    diagnostics.extend(consts.into_diagnostics());
    program.types = structs;
    (program, diagnostics)
}

/// Checks that each function of the runtime is an intrinsic, declared with its signature.
fn check_intrinsics(module: &hir::Module) -> Vec<Diagnostic> {
    let mut diagnostics = Vec::new();
    let runtime = module
        .functions
        .iter()
        .filter(|(_, function)| function.kind == hir::FnKind::Runtime);
    for (_, function) in runtime {
        let name = &function.name;
        let Some(intrinsic) = intrinsic_of(function) else {
            diagnostics.push(Diagnostic::error(
                codes::INVALID_INTRINSIC,
                format!(
                    "the runtime has no function named `{}`",
                    name.value.trim_start_matches('_')
                ),
                name.span,
            ));
            continue;
        };
        let params: Vec<Option<Ty>> = function
            .params
            .iter()
            .map(|param| (param.convention == Convention::Read).then_some(param.ty.value))
            .collect();
        let expected: Vec<Option<Ty>> = intrinsic
            .params()
            .iter()
            .map(|&kind| Some(kind_ty(kind)))
            .collect();
        if params != expected || function.ret.value != kind_ty(intrinsic.ret()) || function.raises {
            let mut parts = vec![format!(":fn {}", name.value)];
            parts.extend(
                intrinsic
                    .params()
                    .iter()
                    .enumerate()
                    .map(|(index, &kind)| format!("p{index}:{}", kind_ty(kind))),
            );
            if intrinsic.ret() != Kind::Nothing {
                parts.push(format!("-> {}", kind_ty(intrinsic.ret())));
            }
            let expected = parts.join(" ");
            diagnostics.push(
                Diagnostic::error(
                    codes::INVALID_INTRINSIC,
                    format!(
                        "the runtime function `{}` does not match its intrinsic",
                        name.value
                    ),
                    name.span,
                )
                .with_help(format!(
                    "declare it as `{expected}`, with `read` parameters and without `raises`"
                )),
            );
        }
    }
    diagnostics
}

/// The intrinsic a function of the runtime declares: the one named like it, without the `_`
/// that makes the declaration private.
fn intrinsic_of(function: &hir::Function) -> Option<Intrinsic> {
    let name = &function.name.value;
    Intrinsic::from_name(name.strip_prefix('_').unwrap_or(name))
}

/// The Pika type of a kind of value of an intrinsic.
pub fn kind_ty(kind: Kind) -> Ty {
    match kind {
        Kind::Int => Ty::Int(IntTy::I64),
        Kind::Float => Ty::Float(hir::FloatTy::F64),
        Kind::Bool => Ty::Bool,
        Kind::Char => Ty::Char,
        Kind::Str => Ty::String,
        Kind::Duration => Ty::Duration,
        Kind::Nothing => Ty::Nothing,
    }
}

/// Builds the check body of generic function `id`, with its type parameters as types, and
/// returns the ownership problems found in it.
fn check_generic_body<'a>(
    module: &'a hir::Module,
    structs: &'a Types,
    types: &'a TypeckResult,
    consts: &mut ConstValues<'a>,
    id: hir::FnId,
) -> Vec<Diagnostic> {
    let copy_params = module.functions[id]
        .generics
        .params
        .iter()
        .map(|param| {
            param.bounds.iter().any(|bound| {
                pika_types::implied_bounds(module, bound.value)
                    .contains(&hir::Bound::Builtin(hir::BuiltinTrait::Copy))
            })
        })
        .collect();
    let instance = Instance {
        subst: Box::new([]),
        copy_params,
        locate: None,
    };
    // Its calls are not instances of the program.
    let mut unused = Instances::default();
    let (mut body, _, mut errors) =
        build_instance(module, structs, types, consts, &mut unused, id, instance);
    errors.extend(crate::analyze(&mut body));
    errors
}

/// Builds the body of one function instance, or the check body of a generic function.
fn build_instance<'a>(
    module: &'a hir::Module,
    structs: &'a Types,
    types: &'a TypeckResult,
    consts: &mut ConstValues<'a>,
    instances: &mut Instances,
    id: hir::FnId,
    instance: Instance<'a>,
) -> (Body, Vec<Unsupported>, Vec<Diagnostic>) {
    let function = &module.functions[id];
    let mut builder = Builder::new(
        module,
        structs,
        &function.body,
        &types.functions[id],
        consts,
        Mode::Runtime,
    );
    builder.subst = instance.subst;
    builder.copy_params = instance.copy_params;
    builder.instances = Some(instances);
    builder.locate = instance.locate;
    builder.build_function(function)
}

/// How a generic body is built.
struct Instance<'a> {
    /// The type arguments of an instance, or none for a function without type parameters or
    /// for the check body of a generic function.
    subst: Box<[Ty]>,
    /// For a check body, which type parameters are `Copy`.
    copy_params: Vec<bool>,
    /// The line and column of source locations, for errors; not needed in check bodies.
    locate: Option<&'a SourceMap>,
}

/// The deepest nesting of type arguments an instance may have. Deeper ones come from a
/// generic function that calls itself with ever larger types, which has no end.
const MAX_INSTANCE_DEPTH: usize = 32;

/// The function instances of a program, numbered in the order they are first requested.
#[derive(Default)]
struct Instances {
    ids: HashMap<(hir::FnId, Box<[Ty]>, bool), InstanceId>,
    /// Each instance: the function, its type arguments, and whether it is the adapter that
    /// makes the function a value.
    queue: Vec<(hir::FnId, Box<[Ty]>, bool)>,
    errors: Vec<Diagnostic>,
}

impl Instances {
    /// The body that calls function `id`, without type parameters, as the body of a
    /// function value (spec section 11.4).
    fn request_adapter(&mut self, id: hir::FnId) -> InstanceId {
        if let Some(&instance) = self.ids.get(&(id, Box::new([]) as Box<[Ty]>, true)) {
            return instance;
        }
        let instance = self.next_id();
        self.ids.insert((id, Box::new([]), true), instance);
        self.queue.push((id, Box::new([]), true));
        instance
    }

    fn next_id(&self) -> InstanceId {
        InstanceId::from_raw(la_arena::RawIdx::from_u32(
            u32::try_from(self.queue.len()).expect("fewer than 2^32 instances"),
        ))
    }

    /// The instance of function `id` with type arguments `args`, called at `span`.
    fn request(&mut self, id: hir::FnId, args: Box<[Ty]>, span: Span) -> InstanceId {
        if let Some(&instance) = self.ids.get(&(id, args.clone(), false)) {
            return instance;
        }
        let instance = self.next_id();
        if args.iter().any(|&arg| depth(arg) > MAX_INSTANCE_DEPTH) {
            // The instance is not built: the program will not run.
            if self.errors.is_empty() {
                self.errors.push(
                    Diagnostic::error(
                        codes::INSTANCE_TOO_DEEP,
                        "this call needs a generic function instance with ever deeper types",
                        span,
                    )
                    .with_help(format!(
                        "type arguments nested more than {MAX_INSTANCE_DEPTH} deep come from a \
                         generic function that calls itself with larger and larger types"
                    )),
                );
            }
            return InstanceId::from_raw(la_arena::RawIdx::from_u32(0));
        }
        self.ids.insert((id, args.clone(), false), instance);
        self.queue.push((id, args, false));
        instance
    }
}

/// The body that makes function `id` a value: it receives the function value's (empty)
/// captures, then calls the function with its arguments.
fn build_adapter(
    module: &hir::Module,
    structs: &Types,
    instances: &mut Instances,
    id: hir::FnId,
) -> Body {
    let function = &module.functions[id];
    let span = function.name.span;
    let target = instances.request(id, Box::new([]), span);
    let mut locals = Arena::new();
    let ret = function.ret.value;
    let return_local = locals.alloc(LocalDecl {
        ty: ret,
        mode: LocalMode::Value,
        user: None,
    });
    let mut params = Vec::new();
    let mut args = Vec::new();
    for param in &function.params {
        let ty = param.ty.value;
        let copy = structs.is_copy(ty);
        let local = locals.alloc(LocalDecl {
            ty,
            mode: if copy {
                LocalMode::Value
            } else {
                LocalMode::Ref { mutable: false }
            },
            user: None,
        });
        params.push(local);
        args.push(if copy {
            CallArg::Value(Operand::Copy {
                place: Place::Local(local),
                span,
            })
        } else {
            CallArg::Ref {
                place: Place::Local(local),
                mutable: false,
                span,
            }
        });
    }
    let mut blocks = Arena::new();
    let entry = blocks.alloc(BasicBlock {
        statements: Vec::new(),
        terminator: Terminator::Unreachable,
    });
    let target_block = (ret != Ty::Never).then(|| {
        blocks.alloc(BasicBlock {
            statements: Vec::new(),
            terminator: Terminator::Return,
        })
    });
    let on_error = function.raises.then(|| {
        let error = locals.alloc(LocalDecl {
            ty: structs.error_ty(),
            mode: LocalMode::Value,
            user: None,
        });
        let block = blocks.alloc(BasicBlock {
            statements: Vec::new(),
            terminator: Terminator::Raise(Operand::Move {
                place: Place::Local(error),
                span,
            }),
        });
        ErrorTarget {
            place: Place::Local(error),
            block,
        }
    });
    blocks[entry].terminator = Terminator::Call {
        func: CallTarget::Direct(target),
        args,
        destination: Place::Local(return_local),
        target: target_block,
        on_error,
    };
    Body {
        name: function.name.value.clone(),
        span,
        locals,
        return_local,
        params,
        blocks,
        entry,
        raises: function.raises,
        env: Some(Vec::new()),
    }
}

/// Requests the instances of the `drop` and `fmt` functions that destroying and displaying a
/// value of type `ty` run.
fn request_impls(structs: &Types, instances: &mut Instances, program: &mut Program, ty: Ty) {
    let impls = structs
        .user_drops(ty)
        .into_iter()
        .map(|dropped| {
            (
                dropped,
                structs.adt(dropped).and_then(|info| info.drop),
                true,
            )
        })
        .chain(
            structs
                .user_displays(ty)
                .into_iter()
                .map(|shown| (shown, structs.adt(shown).and_then(|info| info.fmt), false)),
        );
    for (ty, function, is_drop) in impls {
        let map = if is_drop {
            &mut program.drop_fns
        } else {
            &mut program.display_fns
        };
        if map.contains_key(&ty) {
            continue;
        }
        let function = function.expect("the type has the function");
        let instance = instances.request(function, ty.components().into(), Span::default());
        map.insert(ty, instance);
    }
}

/// The function a call of function `id` with type arguments `args` runs. A function of a
/// trait called on a struct or enum runs that type's own version, if it defines one, and
/// otherwise the trait's default version.
fn resolve_trait_call(
    module: &hir::Module,
    id: hir::FnId,
    args: Box<[Ty]>,
) -> (hir::FnId, Box<[Ty]>) {
    let function = &module.functions[id];
    let (Some(hir::FnOwner::Trait(_)), Some(Ty::Adt(adt))) = (function.owner, args.first()) else {
        return (id, args);
    };
    let ty = hir::TypeId::from_raw(la_arena::RawIdx::from_u32(adt.index));
    match module.types[ty].function(&function.name.value) {
        Some(own) => {
            let own_args = adt.args.iter().chain(&args[1..]).copied().collect();
            (own, own_args)
        }
        None => (id, args),
    }
}

/// For a call of a prelude trait's function on a built-in type, such as the `add` of `i64`:
/// the trait, whose function is a built-in operation.
fn builtin_operator(module: &hir::Module, id: hir::FnId, args: &[Ty]) -> Option<hir::PreludeTrait> {
    let function = &module.functions[id];
    let Some(hir::FnOwner::Trait(t)) = function.owner else {
        return None;
    };
    let prelude = module.traits[t].prelude?;
    let builtin = args
        .first()
        .is_some_and(|ty| !matches!(ty, Ty::Adt(_) | Ty::Param(_)));
    builtin.then_some(prelude)
}

/// How deeply type arguments nest in a type.
fn depth(ty: Ty) -> usize {
    1 + ty.components().into_iter().map(depth).max().unwrap_or(0)
}

/// An `:onerror` whose body is being lowered: errors raised in it go to its handler.
struct Handler {
    /// The handler's variable, which receives the error.
    error: LocalId,
    /// The handler's first block.
    block: BlockId,
    /// The number of scopes open outside the body; raising drops the ones inside.
    scope_depth: usize,
    /// The number of temporaries outside the body; raising drops the ones created inside.
    temps_depth: usize,
}

/// What the MIR is built for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A function of the program.
    Runtime,
    /// A constant initializer, evaluated by the interpreter at compile time.
    ConstEval,
}

/// What borrows a place while code runs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Borrower {
    /// The names of a `:match` arm.
    Match,
    /// The variables of a `:foreach` loop.
    Foreach,
}

/// The parts of a `:for` loop needed after its setup.
struct ForParts {
    var: LocalId,
    int: IntTy,
    end: Operand,
    step: Operand,
    positive: Operand,
    end_kind: ForEnd,
    span: Span,
}

struct LoopTargets {
    break_block: BlockId,
    continue_block: BlockId,
    /// The number of scopes open outside the loop; leaving the loop drops the ones inside.
    scope_depth: usize,
}

pub(crate) struct Builder<'a, 'c> {
    module: &'a hir::Module,
    /// The program's user-defined types.
    structs: &'a Types,
    hir: &'a hir::Body,
    types: &'a BodyTypes,
    consts: &'c mut ConstValues<'a>,
    mode: Mode,
    locals: Arena<LocalDecl>,
    blocks: Arena<BasicBlock>,
    /// Blocks whose terminator has been set.
    terminated: ArenaMap<BlockId, ()>,
    /// The block being filled, or `None` after a terminator (in unreachable code).
    current: Option<BlockId>,
    local_map: ArenaMap<hir::LocalId, LocalId>,
    /// Owned locals of each open scope, outermost first, to drop when the scope ends.
    scopes: Vec<Vec<LocalId>>,
    /// Owned temporaries of the current full expression, dropped when it ends.
    temps: Vec<LocalId>,
    loops: Vec<LoopTargets>,
    /// Places borrowed by the bindings of the `:match` arms being lowered, with the span of
    /// the value matched: they cannot be modified or moved until the arm ends.
    match_borrows: Vec<(Place, Span, Borrower)>,
    /// Whether reads of variables are taken now, into temporaries, rather than when the
    /// operand is used: when a later operand of the same operation may change them.
    eager_reads: bool,
    return_local: Option<LocalId>,
    /// The type arguments of the instance being built, by type parameter index.
    subst: Box<[Ty]>,
    /// In the check body of a generic function, which type parameters are `Copy`.
    copy_params: Vec<bool>,
    /// The instances of called functions; absent in constant initializers, which call none.
    instances: Option<&'c mut Instances>,
    /// Whether the function being built is declared `raises`.
    raises: bool,
    /// Whether the body is the body of a function value, which receives its captures.
    is_closure: bool,
    /// For the body of a function value: the types of its captures.
    env: Vec<Ty>,
    /// The variables of the source that are captures, with their positions.
    capture_map: ArenaMap<hir::LocalId, u32>,
    /// The `:onerror` statements whose bodies are being lowered, innermost last.
    handlers: Vec<Handler>,
    /// The line and column of a source location, recorded in errors.
    locate: Option<&'a SourceMap>,
    unsupported: Vec<Unsupported>,
    diagnostics: Vec<Diagnostic>,
}

impl<'a, 'c> Builder<'a, 'c> {
    pub(crate) fn new(
        module: &'a hir::Module,
        structs: &'a Types,
        hir: &'a hir::Body,
        types: &'a BodyTypes,
        consts: &'c mut ConstValues<'a>,
        mode: Mode,
    ) -> Self {
        Self {
            module,
            structs,
            hir,
            types,
            consts,
            mode,
            locals: Arena::new(),
            blocks: Arena::new(),
            terminated: ArenaMap::default(),
            current: None,
            local_map: ArenaMap::default(),
            scopes: Vec::new(),
            temps: Vec::new(),
            loops: Vec::new(),
            match_borrows: Vec::new(),
            eager_reads: false,
            return_local: None,
            subst: Box::new([]),
            copy_params: Vec::new(),
            instances: None,
            raises: false,
            is_closure: false,
            env: Vec::new(),
            capture_map: ArenaMap::default(),
            handlers: Vec::new(),
            locate: None,
            unsupported: Vec::new(),
            diagnostics: Vec::new(),
        }
    }

    // ----- Building blocks -----------------------------------------------------------------

    fn new_block(&mut self) -> BlockId {
        self.blocks.alloc(BasicBlock {
            statements: Vec::new(),
            terminator: Terminator::Unreachable,
        })
    }

    /// The block being filled; code after a terminator goes into a fresh, unreachable block.
    fn current_block(&mut self) -> BlockId {
        if let Some(block) = self.current {
            return block;
        }
        let block = self.new_block();
        self.current = Some(block);
        block
    }

    fn push(&mut self, statement: Statement) {
        let block = self.current_block();
        self.blocks[block].statements.push(statement);
    }

    fn terminate(&mut self, terminator: Terminator) {
        let block = self.current_block();
        self.blocks[block].terminator = terminator;
        self.terminated.insert(block, ());
        self.current = None;
    }

    /// Branches on a `bool` operand. A constant condition becomes a jump, so that analyses
    /// see the same control flow as the type checker (`:while (true)` only exits by `:break`).
    fn branch(&mut self, cond: Operand, then_block: BlockId, else_block: BlockId) {
        match cond {
            Operand::Const(Value::Bool(true)) => self.goto(then_block),
            Operand::Const(Value::Bool(false)) => self.goto(else_block),
            cond => self.terminate(Terminator::If {
                cond,
                then_block,
                else_block,
            }),
        }
    }

    fn goto(&mut self, target: BlockId) {
        self.terminate(Terminator::Goto(target));
    }

    fn switch_to(&mut self, block: BlockId) {
        debug_assert!(self.current.is_none(), "switching away from an open block");
        self.current = Some(block);
    }

    fn alloc_local(&mut self, ty: Ty, mode: LocalMode, user: Option<UserVariable>) -> LocalId {
        self.locals.alloc(LocalDecl { ty, mode, user })
    }

    /// A new temporary. Temporaries that own resources are dropped when the current full
    /// expression ends.
    fn temp(&mut self, ty: Ty) -> LocalId {
        let temp = self.alloc_local(ty, LocalMode::Value, None);
        if self.needs_drop(ty) {
            self.temps.push(temp);
        }
        temp
    }

    fn assign(&mut self, place: Place, value: Rvalue, span: Span) {
        self.push(Statement::Assign { place, value, span });
    }

    /// Assigns `value` to a new temporary and returns a read of it.
    fn assign_temp(&mut self, ty: Ty, value: Rvalue, span: Span) -> Operand {
        let temp = self.temp(ty);
        self.assign(Place::Local(temp), value, span);
        Operand::Copy {
            place: Place::Local(temp),
            span,
        }
    }

    fn finish(mut self, params: Vec<LocalId>, name: String, span: Span) -> Body {
        let return_local = self.return_local.expect("set before building the body");
        if self.current.is_some() {
            self.terminate(Terminator::Return);
        }
        let entry = self
            .blocks
            .iter()
            .next()
            .map(|(id, _)| id)
            .expect("the entry block exists");
        debug_assert!(
            self.blocks.iter().all(|(id, block)| self.terminated.get(id).is_some()
                || block.statements.is_empty()),
            "a block with statements was never terminated"
        );
        Body {
            name,
            span,
            locals: self.locals,
            return_local,
            params,
            blocks: self.blocks,
            entry,
            raises: self.raises,
            env: self.is_closure.then(|| self.env.clone()),
        }
    }

    fn error(&mut self, diagnostic: Diagnostic) {
        if self.mode == Mode::Runtime {
            self.diagnostics.push(diagnostic);
        }
    }

    /// Warns about `:local` variables that are assigned once and never modified, by `:set` or
    /// through a `mut` parameter: they could be `:const` (spec section 6).
    fn lint_could_be_const(&mut self) {
        let mut writes: ArenaMap<LocalId, usize> = ArenaMap::default();
        for (_, block) in self.blocks.iter() {
            for statement in &block.statements {
                // A change to a part of a variable modifies it, whatever the
                // initialization: it counts as a second write.
                let (place, count) = match statement {
                    Statement::Assign { place, value, .. } => match value {
                        Rvalue::ListPop(changed)
                        | Rvalue::ListRemove { list: changed, .. }
                        | Rvalue::MapInsert { map: changed, .. }
                        | Rvalue::MapRemove { map: changed, .. } => (changed, 2),
                        _ if place.as_local().is_some() => (place, 1),
                        _ => (place, 2),
                    },
                    Statement::ListPush { list: place, .. }
                    | Statement::ListInsert { list: place, .. }
                    | Statement::ListSwap { list: place, .. }
                    | Statement::Clear(place) => (place, 2),
                    Statement::BindRef { local, place }
                        if self.locals[*local].mode == (LocalMode::Ref { mutable: true }) =>
                    {
                        (place, 2)
                    }
                    _ => continue,
                };
                if let PlaceRoot::Local(local) = place.root() {
                    *writes.entry(local).or_default() += count;
                }
            }
            if let Terminator::Call { args, .. } = &block.terminator {
                for arg in args {
                    if let CallArg::Ref {
                        place,
                        mutable: true,
                        ..
                    } = arg
                        && let PlaceRoot::Local(local) = place.root()
                    {
                        // Counts as a second write, whatever the initialization.
                        *writes.entry(local).or_default() += 2;
                    }
                }
            }
        }
        for (hir_local, &local) in self.local_map.iter() {
            let decl = &self.hir.locals[hir_local];
            let declared_with_value = self.locals[local]
                .user
                .as_ref()
                .is_some_and(|user| !user.declared_without_value);
            if decl.kind == hir::LocalKind::Var
                && declared_with_value
                && writes.get(local).copied().unwrap_or(0) <= 1
            {
                self.diagnostics.push(
                    Diagnostic::warning(
                        codes::COULD_BE_CONST,
                        format!("variable `{}` is never modified", decl.name.value),
                        decl.name.span,
                    )
                    .with_help(format!("declare it with `:const {}`", decl.name.value)),
                );
            }
        }
    }

    // ----- Scopes and drops --------------------------------------------------------------

    fn push_scope(&mut self) {
        self.scopes.push(Vec::new());
    }

    /// Closes the innermost scope, dropping its owned locals if the end is reachable.
    fn pop_scope(&mut self) {
        let locals = self.scopes.pop().expect("a scope is open");
        if self.current.is_some() {
            for local in locals.into_iter().rev() {
                self.drop_local(local);
            }
        }
    }

    /// Drops the owned locals of the scopes from `depth` inward, innermost first, without
    /// closing them: used before jumping out of them.
    fn drop_scopes_from(&mut self, depth: usize) {
        let locals: Vec<LocalId> = self.scopes[depth..]
            .iter()
            .rev()
            .flat_map(|scope| scope.iter().rev().copied())
            .collect();
        for local in locals {
            self.drop_local(local);
        }
    }

    fn drop_local(&mut self, local: LocalId) {
        self.push(Statement::Drop {
            place: Place::Local(local),
            flag: None,
        });
    }

    /// Runs `f` as a full expression: temporaries it creates are dropped afterwards.
    fn full_expression<R>(&mut self, f: impl FnOnce(&mut Self) -> R) -> R {
        let outer = self.temps.len();
        let result = f(self);
        let temps = self.temps.split_off(outer);
        if self.current.is_some() {
            for temp in temps.into_iter().rev() {
                self.drop_local(temp);
            }
        }
        result
    }

    /// Drops the temporaries of the enclosing full expressions; used before jumping away.
    fn drop_all_temps(&mut self) {
        for temp in self.temps.clone().into_iter().rev() {
            self.drop_local(temp);
        }
    }

    // ----- Bodies ------------------------------------------------------------------------

    fn build_function(
        mut self,
        function: &hir::Function,
    ) -> (Body, Vec<Unsupported>, Vec<Diagnostic>) {
        let ret_ty = self.ty(function.ret.value);
        self.raises = function.raises;
        if let hir::FnKind::Closure(_) = function.kind {
            self.is_closure = true;
            for (index, capture) in function.captures.iter().enumerate() {
                self.env.push(self.local_ty(capture.inner));
                self.capture_map
                    .insert(capture.inner, u32::try_from(index).expect("few captures"));
            }
        }
        self.return_local = Some(self.alloc_local(ret_ty, LocalMode::Value, None));
        let entry = self.new_block();
        self.switch_to(entry);
        self.push_scope();
        let mut params = Vec::new();
        for param in &function.params {
            let ty = self.local_ty(param.local);
            let mode = match param.convention {
                Convention::Mut => LocalMode::Ref { mutable: true },
                Convention::Read if !self.is_copy(ty) => LocalMode::Ref { mutable: false },
                Convention::Read | Convention::Owned => LocalMode::Value,
            };
            params.push(self.declare_user_local(param.local, mode, false));
        }
        self.lower_block(&function.root);
        self.pop_scope();
        self.lint_could_be_const();
        let unsupported = std::mem::take(&mut self.unsupported);
        let diagnostics = std::mem::take(&mut self.diagnostics);
        let name = function.name.value.clone();
        (
            self.finish(params, name, function.name.span),
            unsupported,
            diagnostics,
        )
    }

    /// Builds the MIR of a constant initializer: an expression whose value is returned.
    pub(crate) fn build_const(mut self, expr: hir::ExprId, name: &str) -> Body {
        let ty = self.expr_ty(expr);
        let span = self.hir.expr_span(expr);
        self.return_local = Some(self.alloc_local(ty, LocalMode::Value, None));
        let entry = self.new_block();
        self.switch_to(entry);
        let return_local = self.return_local.expect("set above");
        self.full_expression(|b| {
            let value = b.lower_consume(expr);
            b.assign(Place::Local(return_local), Rvalue::Use(value), span);
        });
        self.finish(Vec::new(), name.to_owned(), span)
    }

    /// Declares the MIR local of a source variable. Owned values are dropped at the end of the
    /// current scope.
    fn declare_user_local(
        &mut self,
        local: hir::LocalId,
        mode: LocalMode,
        declared_without_value: bool,
    ) -> LocalId {
        let decl = &self.hir.locals[local];
        let ty = self.local_ty(local);
        let id = self.alloc_local(
            ty,
            mode,
            Some(UserVariable {
                name: decl.name.value.clone(),
                span: decl.name.span,
                declared_without_value,
            }),
        );
        self.local_map.insert(local, id);
        if mode == LocalMode::Value && self.needs_drop(ty) {
            self.scopes.last_mut().expect("a scope is open").push(id);
        }
        id
    }

    /// The type of an expression's value as used: an option when it is implicitly wrapped.
    fn expr_ty(&self, expr: hir::ExprId) -> Ty {
        match self.wrapped(expr) {
            Some(option) => option,
            None => self.own_ty(expr),
        }
    }

    /// The type of an expression's own value, before any wrapping.
    fn own_ty(&self, expr: hir::ExprId) -> Ty {
        self.ty(self.types.exprs.get(expr).copied().unwrap_or(Ty::Error))
    }

    /// The option an expression's value is implicitly wrapped in, if it is.
    fn wrapped(&self, expr: hir::ExprId) -> Option<Ty> {
        self.types.wrapped.get(expr).map(|&option| self.ty(option))
    }

    /// The type of a variable.
    fn local_ty(&self, local: hir::LocalId) -> Ty {
        self.ty(self.types.locals.get(local).copied().unwrap_or(Ty::Error))
    }

    /// The type of the value a pattern matches.
    fn pat_ty(&self, pat: hir::PatId) -> Ty {
        self.ty(self.types.pats.get(pat).copied().unwrap_or(Ty::Error))
    }

    /// A type of the generic body, in the instance being built.
    fn ty(&self, ty: Ty) -> Ty {
        ty.subst(&self.subst)
    }

    /// Returns true for types whose values are copied rather than moved.
    fn is_copy(&self, ty: Ty) -> bool {
        self.structs.is_copy_in(ty, &self.copy_params)
    }

    /// Returns true for types whose values may own resources that must be freed.
    fn needs_drop(&self, ty: Ty) -> bool {
        self.structs.needs_drop_in(ty, &self.copy_params)
    }

    // ----- Statements --------------------------------------------------------------------

    fn lower_block(&mut self, block: &hir::Block) {
        self.push_scope();
        for &stmt in &block.stmts {
            self.lower_stmt(stmt);
        }
        self.pop_scope();
    }

    fn lower_stmt(&mut self, stmt: hir::StmtId) {
        let span = self.hir.stmt_span(stmt);
        match &self.hir.stmts[stmt] {
            hir::Stmt::Expr(expr) => {
                self.full_expression(|b| {
                    b.lower_read(*expr);
                });
            }
            hir::Stmt::Let { local, init } => {
                let value = init.map(|init| {
                    let outer = self.temps.len();
                    let value = self.lower_consume(init);
                    (value, outer)
                });
                let id = self.declare_user_local(*local, LocalMode::Value, init.is_none());
                if let Some((value, outer)) = value {
                    self.assign(Place::Local(id), Rvalue::Use(value), span);
                    self.drop_temps_since(outer);
                }
            }
            &hir::Stmt::Set { target, value } => self.lower_set(target, value, span),
            hir::Stmt::Match { scrutinee, arms } => self.lower_match(*scrutinee, arms),
            &hir::Stmt::Foreach {
                key,
                value,
                collection,
                ref body,
            } => self.lower_foreach(key, value, collection, body),
            &hir::Stmt::OnError {
                error,
                ref body,
                ref handler,
            } => self.lower_onerror(error, body, handler),
            hir::Stmt::If {
                cond,
                then_block,
                else_branch,
            } => self.lower_if(*cond, then_block, else_branch.as_ref()),
            hir::Stmt::While { cond, body } => {
                let header = self.new_block();
                let body_block = self.new_block();
                let exit = self.new_block();
                self.goto(header);
                self.switch_to(header);
                let cond = self.lower_condition(*cond);
                self.branch(cond, body_block, exit);
                self.switch_to(body_block);
                self.lower_loop_body(body, exit, header);
                if self.current.is_some() {
                    self.goto(header);
                }
                self.switch_to(exit);
            }
            hir::Stmt::DoWhile { body, cond } => {
                let body_block = self.new_block();
                let cond_block = self.new_block();
                let exit = self.new_block();
                self.goto(body_block);
                self.switch_to(body_block);
                self.lower_loop_body(body, exit, cond_block);
                if self.current.is_some() {
                    self.goto(cond_block);
                }
                self.switch_to(cond_block);
                let cond = self.lower_condition(*cond);
                self.branch(cond, body_block, exit);
                self.switch_to(exit);
            }
            hir::Stmt::For {
                var,
                from,
                end,
                step,
                body,
            } => {
                self.push_scope();
                self.lower_for(*var, *from, *end, *step, body, span);
                self.pop_scope();
            }
            hir::Stmt::Block(block) => self.lower_block(block),
        }
    }

    /// `:set`: the new value is computed, the old one destroyed, then the new one stored.
    fn lower_set(&mut self, target: hir::Place, value_expr: hir::ExprId, span: Span) {
        self.full_expression(|b| {
            let mut value = b.lower_consume(value_expr);
            // `:set ($map->key) value` inserts the key if it is missing.
            if let hir::Place::Part(part) = target
                && let hir::Expr::Index { base, index } = b.hir.exprs[part]
                && let Ty::Map(_) = b.own_ty(base)
            {
                let Some(map) = b.place_of(base) else {
                    return;
                };
                b.check_not_borrowed(&map, span);
                let key = b.lower_consume(index);
                let old = Ty::option(b.own_ty(part));
                b.assign_temp(old, Rvalue::MapInsert { map, key, value }, span);
                return;
            }
            let place = match target {
                hir::Place::Local(local) => b.local_place(local),
                hir::Place::Global(global) => Place::Global(global),
                hir::Place::Part(part) => match b.place_of(part) {
                    Some(place) => place,
                    None => return,
                },
                hir::Place::Error => return,
            };
            b.check_not_borrowed(&place, span);
            let needs_drop = b.structs.needs_drop(b.place_ty(&place));
            let ty = b.expr_ty(value_expr);
            match value {
                // The new value is moved out before the old one is destroyed: in
                // `:set s $s`, the move must happen first.
                Operand::Move {
                    span: value_span, ..
                } => {
                    let temp = b.assign_temp(ty, Rvalue::Use(value), value_span);
                    value = to_move(temp);
                }
                // Likewise, a copy of a part of the target is taken before the target is
                // destroyed.
                Operand::Copy {
                    span: value_span, ..
                } if needs_drop => {
                    value = b.assign_temp(ty, Rvalue::Use(value), value_span);
                }
                _ => {}
            }
            if needs_drop {
                b.push(Statement::Drop {
                    place: place.clone(),
                    flag: None,
                });
            }
            b.assign(place, Rvalue::Use(value), span);
        });
    }

    /// `:match`. The value matched is a place (a variable or a part of one), whose parts
    /// the arms' names borrow, or a temporary that the arms' names take parts of. Each arm
    /// tests its pattern and continues with the next arm if it does not match; the type
    /// checker guarantees that some arm matches.
    fn lower_match(&mut self, scrutinee: hir::ExprId, arms: &[hir::MatchArm]) {
        let scrutinee_span = self.hir.expr_span(scrutinee);
        // The scope of the temporary holding a value that is not a place.
        self.push_scope();
        let (place, owned) = self.subject_place(scrutinee);
        let join = self.new_block();
        for arm in arms {
            let next = self.new_block();
            self.lower_pattern_test(arm.pat, &place, next);
            self.lower_arm(arm, &place, owned, next, scrutinee_span);
            if self.current.is_some() {
                self.goto(join);
            }
            self.switch_to(next);
        }
        self.terminate(Terminator::Unreachable);
        self.switch_to(join);
        self.pop_scope();
    }

    /// `:foreach`: the variables refer to (or copy) each element, or each key and value, in
    /// order. The collection is borrowed by the loop: the body cannot change or move it.
    fn lower_foreach(
        &mut self,
        key: Option<hir::LocalId>,
        value: hir::LocalId,
        collection: hir::ExprId,
        body: &hir::Block,
    ) {
        let ty = self.expr_ty(collection);
        let span = self.hir.expr_span(collection);
        self.push_scope();
        let (place, _) = self.subject_place(collection);
        let len = self.temp(INT);
        self.assign(Place::Local(len), Rvalue::Len(place.clone()), span);
        let position = self.temp(INT);
        self.assign(Place::Local(position), Rvalue::Use(int_const(0)), span);
        let (header, body_block, latch, exit) = (
            self.new_block(),
            self.new_block(),
            self.new_block(),
            self.new_block(),
        );
        self.goto(header);
        self.switch_to(header);
        let at = Operand::Copy {
            place: Place::Local(position),
            span,
        };
        let more = self.binary_temp(
            BinaryOp::Lt,
            at.clone(),
            Operand::Copy {
                place: Place::Local(len),
                span,
            },
            Ty::Bool,
            span,
        );
        self.branch(more, body_block, exit);
        self.switch_to(body_block);
        self.push_scope();
        let (key_place, value_place) = match ty {
            Ty::List(_) => (None, Place::Index(Box::new(place.clone()), position)),
            Ty::Map(_) => (
                Some(Place::MapKey(Box::new(place.clone()), position)),
                Place::MapValue(Box::new(place.clone()), position),
            ),
            _ => (None, Place::MapKey(Box::new(place.clone()), position)),
        };
        if let Some(key) = key {
            if let Some(key_place) = key_place {
                self.bind_element(key, key_place, false);
            } else {
                let id = self.declare_user_local(key, LocalMode::Value, false);
                self.assign(Place::Local(id), Rvalue::Use(at.clone()), span);
            }
        }
        let mutable = matches!(
            self.hir.locals[value].kind,
            hir::LocalKind::Element { mutable: true }
        );
        self.bind_element(value, value_place, mutable);
        self.match_borrows.push((place, span, Borrower::Foreach));
        self.lower_loop_body(body, exit, latch);
        self.match_borrows.pop();
        self.pop_scope();
        if self.current.is_some() {
            self.goto(latch);
        }
        self.switch_to(latch);
        let next = self.binary_temp(BinaryOp::WrappingAdd, at, int_const(1), INT, span);
        self.assign(Place::Local(position), Rvalue::Use(next), span);
        self.goto(header);
        self.switch_to(exit);
        self.pop_scope();
    }

    /// Declares a `:foreach` variable for an element: a copy of a `Copy` value, otherwise a
    /// reference to it (which may modify it when `mutable`).
    fn bind_element(&mut self, local: hir::LocalId, place: Place, mutable: bool) {
        let ty = self.local_ty(local);
        if !mutable && self.is_copy(ty) {
            let id = self.declare_user_local(local, LocalMode::Value, false);
            let span = self.hir.locals[local].name.span;
            self.assign(
                Place::Local(id),
                Rvalue::Use(Operand::Copy { place, span }),
                span,
            );
        } else {
            let id = self.declare_user_local(local, LocalMode::Ref { mutable }, false);
            self.push(Statement::BindRef { local: id, place });
        }
    }

    /// The place of the value a `:match` or `:foreach` works on, and whether the code owns it.
    /// A variable or a part of one is used in place. A global is copied, like any read of a
    /// global, and the copy is used in place. Any other value is evaluated into a temporary,
    /// which the code owns; it is destroyed when the innermost scope, which the caller opens,
    /// ends.
    fn subject_place(&mut self, subject: hir::ExprId) -> (Place, bool) {
        let ty = self.expr_ty(subject);
        let span = self.hir.expr_span(subject);
        let temp = self.alloc_local(ty, LocalMode::Value, None);
        let found = self.full_expression(|b| {
            let (value, owned) = match b.place_of(subject) {
                Some(place) if matches!(place.root(), PlaceRoot::Local(_)) => return Ok(place),
                Some(place) => (b.read_global_part(place, ty, span), false),
                None => (b.lower_consume(subject), true),
            };
            b.assign(Place::Local(temp), Rvalue::Use(value), span);
            Err(owned)
        });
        match found {
            Ok(place) => (place, false),
            Err(owned) => {
                if self.needs_drop(ty) {
                    self.scopes.last_mut().expect("a scope is open").push(temp);
                }
                (Place::Local(temp), owned)
            }
        }
    }

    /// The bindings, guard and body of an arm whose pattern matched; `next` is where a
    /// failed guard continues.
    fn lower_arm(
        &mut self,
        arm: &hir::MatchArm,
        place: &Place,
        owned: bool,
        next: BlockId,
        scrutinee_span: Span,
    ) {
        let mut bindings = Vec::new();
        self.collect_bindings(arm.pat, place, &mut bindings);
        let takes_parts = owned
            && bindings
                .iter()
                .any(|(local, _)| !self.is_copy(self.local_ty(*local)));
        let borrows = !owned
            && bindings
                .iter()
                .any(|(local, _)| !self.is_copy(self.local_ty(*local)));
        self.push_scope();
        if borrows {
            self.match_borrows
                .push((place.clone(), scrutinee_span, Borrower::Match));
        }
        // Copies, and borrows until the arm is chosen.
        for (local, part) in &bindings {
            let ty = self.local_ty(*local);
            if self.is_copy(ty) {
                let id = self.declare_user_local(*local, LocalMode::Value, false);
                let span = self.hir.locals[*local].name.span;
                self.assign(
                    Place::Local(id),
                    Rvalue::Use(Operand::Copy {
                        place: part.clone(),
                        span,
                    }),
                    span,
                );
            } else {
                let id = self.declare_user_local(*local, LocalMode::Ref { mutable: false }, false);
                self.push(Statement::BindRef {
                    local: id,
                    place: part.clone(),
                });
            }
        }
        if let Some(guard) = arm.guard {
            let cond = self.lower_condition(guard);
            let body = self.new_block();
            self.branch(cond, body, next);
            self.switch_to(body);
        }
        if takes_parts {
            // The arm owns the parts it names: they are moved out of the temporary, and the
            // rest of it is destroyed. A name for the whole value simply takes it.
            let whole = bindings.iter().any(|(_, part)| part == place);
            for (local, part) in &bindings {
                let ty = self.local_ty(*local);
                if self.is_copy(ty) {
                    continue;
                }
                self.check_move_out_of_drop(*local, part);
                let id = self.declare_user_local(*local, LocalMode::Value, false);
                let span = self.hir.locals[*local].name.span;
                self.assign(
                    Place::Local(id),
                    Rvalue::Use(Operand::Move {
                        place: part.clone(),
                        span,
                    }),
                    span,
                );
            }
            if !whole {
                self.drop_unbound(arm.pat, place);
                let temp = place
                    .as_local()
                    .expect("an owned value matched is a temporary");
                self.push(Statement::MarkMoved(temp));
            }
        }
        self.lower_block(&arm.body);
        if borrows {
            self.match_borrows.pop();
        }
        self.pop_scope();
    }

    /// Tests whether the value in `place` matches a pattern, continuing in `fail` if not.
    fn lower_pattern_test(&mut self, pat: hir::PatId, place: &Place, fail: BlockId) {
        let span = self.hir.pat_span(pat);
        let (variant, fields) = match &self.hir.pats[pat] {
            hir::Pat::Invalid(_) | hir::Pat::Wildcard | hir::Pat::Binding(_) => return,
            hir::Pat::Literal(literal) => {
                let ty = self.pat_ty(pat);
                let value = literal_value(literal, ty);
                self.test_equal(place, value, fail, span);
                return;
            }
            hir::Pat::String(text) => {
                self.test_equal(place, Value::Str(text.as_str().into()), fail, span);
                return;
            }
            hir::Pat::None => (0, Vec::new()),
            &hir::Pat::Some(inner) => (1, vec![inner]),
            hir::Pat::Variant {
                variant, fields, ..
            } => (
                u32::try_from(*variant).expect("enums have few variants"),
                fields.clone(),
            ),
        };
        let discriminant = self.discriminant(place.clone(), span);
        let matches = self.binary_temp(
            BinaryOp::Eq,
            discriminant,
            variant_const(variant),
            Ty::Bool,
            span,
        );
        let next = self.new_block();
        self.branch(matches, next, fail);
        self.switch_to(next);
        for (index, field) in fields.into_iter().enumerate() {
            let index = u32::try_from(index).expect("variants have few fields");
            self.lower_pattern_test(field, &place.variant_field(variant, index), fail);
        }
    }

    fn test_equal(&mut self, place: &Place, value: Value, fail: BlockId, span: Span) {
        let current = Operand::Copy {
            place: place.clone(),
            span,
        };
        let equal = self.binary_temp(BinaryOp::Eq, current, Operand::Const(value), Ty::Bool, span);
        let next = self.new_block();
        self.branch(equal, next, fail);
        self.switch_to(next);
    }

    /// The names a pattern binds, with the part of `place` each one names.
    fn collect_bindings(
        &self,
        pat: hir::PatId,
        place: &Place,
        bindings: &mut Vec<(hir::LocalId, Place)>,
    ) {
        match &self.hir.pats[pat] {
            &hir::Pat::Binding(local) => bindings.push((local, place.clone())),
            &hir::Pat::Some(inner) => {
                self.collect_bindings(inner, &place.variant_field(1, 0), bindings);
            }
            hir::Pat::Variant {
                variant, fields, ..
            } => {
                let variant = u32::try_from(*variant).expect("enums have few variants");
                for (index, &field) in fields.iter().enumerate() {
                    let index = u32::try_from(index).expect("variants have few fields");
                    self.collect_bindings(field, &place.variant_field(variant, index), bindings);
                }
            }
            hir::Pat::Invalid(_)
            | hir::Pat::Wildcard
            | hir::Pat::Literal(_)
            | hir::Pat::String(_)
            | hir::Pat::None => {}
        }
    }

    /// Destroys the parts of the value in `place` that the pattern does not bind to a name
    /// that takes ownership.
    fn drop_unbound(&mut self, pat: hir::PatId, place: &Place) {
        let ty = self.pat_ty(pat);
        match &self.hir.pats[pat] {
            &hir::Pat::Binding(local) if !self.is_copy(self.local_ty(local)) => {}
            &hir::Pat::Some(inner) => self.drop_unbound(inner, &place.variant_field(1, 0)),
            hir::Pat::Variant {
                variant, fields, ..
            } => {
                let variant = u32::try_from(*variant).expect("enums have few variants");
                for (index, &field) in fields.iter().enumerate() {
                    let index = u32::try_from(index).expect("variants have few fields");
                    self.drop_unbound(field, &place.variant_field(variant, index));
                }
            }
            _ => {
                if self.needs_drop(ty) {
                    self.push(Statement::Drop {
                        place: place.clone(),
                        flag: None,
                    });
                }
            }
        }
    }

    /// Drops the temporaries created since the temporary list had `outer` entries.
    fn drop_temps_since(&mut self, outer: usize) {
        let temps = self.temps.split_off(outer);
        if self.current.is_some() {
            for temp in temps.into_iter().rev() {
                self.drop_local(temp);
            }
        }
    }

    /// Evaluates a condition; its temporaries are dropped before branching.
    fn lower_condition(&mut self, cond: hir::ExprId) -> Operand {
        self.full_expression(|b| {
            let value = b.lower_read(cond);
            // A condition that is not a constant is copied out before its temporaries go.
            match value {
                Operand::Const(_) => value,
                value => b.assign_temp(Ty::Bool, Rvalue::Use(value), b.hir.expr_span(cond)),
            }
        })
    }

    fn lower_if(
        &mut self,
        cond: hir::ExprId,
        then_block: &hir::Block,
        else_branch: Option<&ElseBranch>,
    ) {
        let cond = self.lower_condition(cond);
        let then_id = self.new_block();
        let else_id = self.new_block();
        let join = self.new_block();
        self.branch(cond, then_id, else_id);
        self.switch_to(then_id);
        self.lower_block(then_block);
        if self.current.is_some() {
            self.goto(join);
        }
        self.switch_to(else_id);
        match else_branch {
            Some(ElseBranch::Block(block)) => self.lower_block(block),
            Some(ElseBranch::If(nested)) => self.lower_stmt(*nested),
            None => {}
        }
        if self.current.is_some() {
            self.goto(join);
        }
        self.switch_to(join);
    }

    fn lower_loop_body(
        &mut self,
        body: &hir::Block,
        break_block: BlockId,
        continue_block: BlockId,
    ) {
        self.loops.push(LoopTargets {
            break_block,
            continue_block,
            scope_depth: self.scopes.len(),
        });
        self.lower_block(body);
        self.loops.pop();
    }

    /// Lowers a `:for` loop. The loop never computes a value past its bound, so it cannot
    /// overflow: after each iteration it compares the remaining distance to the bound with the
    /// step, as unsigned numbers, before stepping.
    fn lower_for(
        &mut self,
        var: hir::LocalId,
        from: hir::ExprId,
        end: Option<(ForEnd, hir::ExprId)>,
        step: Option<hir::ExprId>,
        body: &hir::Block,
        span: Span,
    ) {
        let ty = self.local_ty(var);
        let Ty::Int(int) = ty else {
            return;
        };
        // Evaluate `from`, the bound and the step once, left to right, before the loop.
        let from_value = self.lower_read(from);
        let from = self.assign_temp(ty, Rvalue::Use(from_value), span);
        let Some((end_kind, end)) = end else {
            return;
        };
        let end_value = self.lower_read(end);
        let end = self.assign_temp(ty, Rvalue::Use(end_value), span);
        let step = match step {
            Some(step) => {
                let value = self.lower_read(step);
                self.assign_temp(ty, Rvalue::Use(value), span)
            }
            None => Operand::Const(Value::Int { value: 1, ty: int }),
        };
        let var_local = self.declare_user_local(var, LocalMode::Value, false);
        let i = Operand::Copy {
            place: Place::Local(var_local),
            span,
        };
        self.assign(Place::Local(var_local), Rvalue::Use(from), span);
        let positive = self.step_direction(&step, int, span);
        let parts = ForParts {
            var: var_local,
            int,
            end,
            step,
            positive,
            end_kind,
            span,
        };

        let header = self.new_block();
        let body_block = self.new_block();
        let latch = self.new_block();
        let exit = self.new_block();
        self.goto(header);

        // header: continue while `i <= end` (or `<`, `>=`, `>` depending on bound and direction).
        self.switch_to(header);
        let (up, down) = match end_kind {
            ForEnd::To => (BinaryOp::Le, BinaryOp::Ge),
            ForEnd::Until => (BinaryOp::Lt, BinaryOp::Gt),
        };
        let in_range = self.select_bool(&parts.positive, (up, down), i, parts.end.clone(), span);
        self.branch(in_range, body_block, exit);

        self.switch_to(body_block);
        self.lower_loop_body(body, exit, latch);
        if self.current.is_some() {
            self.goto(latch);
        }

        self.switch_to(latch);
        self.lower_for_latch(&parts, header, exit);
        self.switch_to(exit);
    }

    /// Whether a `:for` loop counts up, as a `bool` operand. A step that is not a constant is
    /// checked for zero at runtime.
    fn step_direction(&mut self, step: &Operand, int: IntTy, span: Span) -> Operand {
        let zero = Operand::Const(Value::Int { value: 0, ty: int });
        if !matches!(step, Operand::Const(_)) {
            let is_zero =
                self.binary_temp(BinaryOp::Eq, step.clone(), zero.clone(), Ty::Bool, span);
            let panic_block = self.new_block();
            let ok_block = self.new_block();
            self.branch(is_zero, panic_block, ok_block);
            self.switch_to(panic_block);
            self.terminate(Terminator::Panic {
                kind: PanicKind::ZeroStep,
                message: Vec::new(),
                span,
            });
            self.switch_to(ok_block);
        }
        match (step, int.is_signed()) {
            (_, false) => Operand::Const(Value::Bool(true)),
            (Operand::Const(Value::Int { value, .. }), true) => {
                Operand::Const(Value::Bool(*value > 0))
            }
            (_, true) => self.binary_temp(BinaryOp::Gt, step.clone(), zero, Ty::Bool, span),
        }
    }

    /// The end of an iteration: stops if the next value would pass the bound, and otherwise
    /// steps and jumps back to `header`. Computing the remaining distance as an unsigned
    /// number cannot overflow, unlike computing the next value first.
    fn lower_for_latch(&mut self, parts: &ForParts, header: BlockId, exit: BlockId) {
        let span = parts.span;
        let unsigned_int = unsigned_of(parts.int);
        let unsigned = Ty::Int(unsigned_int);
        let i = Operand::Copy {
            place: Place::Local(parts.var),
            span,
        };
        let as_unsigned = |b: &mut Self, operand: Operand| {
            b.assign_temp(
                unsigned,
                Rvalue::Cast {
                    kind: CastKind::Reinterpret,
                    operand,
                    to: unsigned,
                },
                span,
            )
        };
        let i_u = as_unsigned(self, i.clone());
        let end_u = as_unsigned(self, parts.end.clone());
        let step_u = as_unsigned(self, parts.step.clone());
        let up_block = self.new_block();
        let down_block = self.new_block();
        let decide = self.new_block();
        let distance = self.temp(unsigned);
        let magnitude = self.temp(unsigned);
        self.branch(parts.positive.clone(), up_block, down_block);

        self.switch_to(up_block);
        self.assign_binary(
            distance,
            BinaryOp::WrappingSub,
            end_u.clone(),
            i_u.clone(),
            span,
        );
        self.assign(Place::Local(magnitude), Rvalue::Use(step_u.clone()), span);
        self.goto(decide);

        self.switch_to(down_block);
        self.assign_binary(distance, BinaryOp::WrappingSub, i_u, end_u, span);
        let zero_u = Operand::Const(Value::Int {
            value: 0,
            ty: unsigned_int,
        });
        self.assign_binary(magnitude, BinaryOp::WrappingSub, zero_u, step_u, span);
        self.goto(decide);

        self.switch_to(decide);
        let done_op = match parts.end_kind {
            ForEnd::To => BinaryOp::Lt,
            ForEnd::Until => BinaryOp::Le,
        };
        let copy = |local| Operand::Copy {
            place: Place::Local(local),
            span,
        };
        let done = self.binary_temp(done_op, copy(distance), copy(magnitude), Ty::Bool, span);
        let step_block = self.new_block();
        self.branch(done, exit, step_block);
        self.switch_to(step_block);
        self.assign_binary(
            parts.var,
            BinaryOp::WrappingAdd,
            i,
            parts.step.clone(),
            span,
        );
        self.goto(header);
    }

    fn assign_binary(
        &mut self,
        dest: LocalId,
        op: BinaryOp,
        lhs: Operand,
        rhs: Operand,
        span: Span,
    ) {
        self.assign(Place::Local(dest), Rvalue::Binary { op, lhs, rhs }, span);
    }

    fn binary_temp(
        &mut self,
        op: BinaryOp,
        lhs: Operand,
        rhs: Operand,
        ty: Ty,
        span: Span,
    ) -> Operand {
        self.assign_temp(ty, Rvalue::Binary { op, lhs, rhs }, span)
    }

    /// `if positive { lhs up rhs } else { lhs down rhs }`, folded when `positive` is constant.
    fn select_bool(
        &mut self,
        positive: &Operand,
        (up, down): (BinaryOp, BinaryOp),
        lhs: Operand,
        rhs: Operand,
        span: Span,
    ) -> Operand {
        if let Operand::Const(Value::Bool(is_up)) = positive {
            let op = if *is_up { up } else { down };
            return self.binary_temp(op, lhs, rhs, Ty::Bool, span);
        }
        let result = self.temp(Ty::Bool);
        let up_block = self.new_block();
        let down_block = self.new_block();
        let join = self.new_block();
        self.branch(positive.clone(), up_block, down_block);
        self.switch_to(up_block);
        self.assign_binary(result, up, lhs.clone(), rhs.clone(), span);
        self.goto(join);
        self.switch_to(down_block);
        self.assign_binary(result, down, lhs, rhs, span);
        self.goto(join);
        self.switch_to(join);
        Operand::Copy {
            place: Place::Local(result),
            span,
        }
    }

    // ----- Places ------------------------------------------------------------------------

    /// Reports moving `part`, bound to `local`, out of a value whose type has a `drop`
    /// function, which must see the value whole.
    fn check_move_out_of_drop(&mut self, local: hir::LocalId, part: &Place) {
        let mut base = part.base();
        while let Some(place) = base {
            let ty = self.place_ty(place);
            if self.structs.adt(ty).is_some_and(|info| info.drop.is_some()) {
                let name = &self.hir.locals[local].name;
                self.diagnostics.push(
                    Diagnostic::error(
                        codes::MOVE_OUT_OF_DROP,
                        format!(
                            "cannot move `{}` out of a value of type `{ty}`, which implements `Drop`",
                            name.value
                        ),
                        name.span,
                    )
                    .with_help(
                        "`drop` needs the whole value; store it in a variable and match the variable, which borrows its parts",
                    ),
                );
                return;
            }
            base = place.base();
        }
    }

    fn place_ty(&self, place: &Place) -> Ty {
        match place {
            Place::Local(local) => self.locals[*local].ty,
            Place::Capture(index) => self.env.get(*index as usize).copied().unwrap_or(Ty::Error),
            Place::Global(global) => self.module.globals[*global]
                .ty
                .as_ref()
                .map_or(Ty::Error, |t| t.value),
            Place::Field(base, index) => self.structs.field_ty(self.place_ty(base), *index),
            Place::VariantField(base, variant, index) => {
                self.structs
                    .variant_field_ty(self.place_ty(base), *variant, *index)
            }
            Place::Deref(base) => match self.place_ty(base) {
                Ty::Box(inner) => *inner,
                _ => Ty::Error,
            },
            Place::Index(base, _) | Place::MapKey(base, _) | Place::MapValue(base, _) => {
                crate::part_ty(self.place_ty(base), place)
            }
        }
    }

    /// The part of `base` that a `->name` access reads, as resolved by the type checker.
    fn project(&self, expr: hir::ExprId, base: &Place) -> Option<Place> {
        Some(match *self.types.projections.get(expr)? {
            Projection::Field(index) => {
                base.field(u32::try_from(index).expect("structs have few fields"))
            }
            Projection::BoxValue => base.deref(),
        })
    }

    /// The place an expression names, if it is a variable or a part of one. The indices
    /// and keys of elements on the way are evaluated and checked, so this is called once for
    /// each use of the expression; nothing is evaluated if it is not a place.
    fn place_of(&mut self, expr: hir::ExprId) -> Option<Place> {
        // A wrapped value is a new option, not the place.
        if self.wrapped(expr).is_some() {
            return None;
        }
        match self.hir.exprs[expr] {
            hir::Expr::Local(local) => Some(self.local_place(local)),
            hir::Expr::Global(global) => Some(Place::Global(global)),
            hir::Expr::Field { base, .. } => {
                let base = self.place_of(base)?;
                self.project(expr, &base)
            }
            hir::Expr::Index { base, index } => {
                let base_place = self.place_of(base)?;
                Some(self.element_place(base_place, self.own_ty(base), index))
            }
            _ => None,
        }
    }

    /// The element of the list, or the value of the map, in `base` at `index`: a list index
    /// is checked to be within bounds, and a map key to be present.
    fn element_place(&mut self, base: Place, base_ty: Ty, index: hir::ExprId) -> Place {
        let span = self.hir.expr_span(index);
        if let Ty::Map(_) = base_ty {
            let key = self.lower_read(index);
            let position = self.temp(INT);
            self.assign(
                Place::Local(position),
                Rvalue::MapFind {
                    map: base.clone(),
                    key,
                },
                span,
            );
            let missing = self.binary_temp(
                BinaryOp::Lt,
                Operand::Copy {
                    place: Place::Local(position),
                    span,
                },
                int_const(0),
                Ty::Bool,
                span,
            );
            self.panic_if(missing, PanicKind::MissingKey, Vec::new(), span);
            return Place::MapValue(Box::new(base), position);
        }
        let position = self.temp(INT);
        let value = self.lower_read(index);
        self.assign(Place::Local(position), Rvalue::Use(value), span);
        self.check_index(&base, position, false, span);
        Place::Index(Box::new(base), position)
    }

    /// Panics with an index-out-of-bounds message unless the `int` in `index` is a valid
    /// index of the list in `list`, or (`allow_end`) its length.
    fn check_index(&mut self, list: &Place, index: LocalId, allow_end: bool, span: Span) {
        let index = Operand::Copy {
            place: Place::Local(index),
            span,
        };
        let len = self.assign_temp(INT, Rvalue::Len(list.clone()), span);
        let message = vec![
            PrintPart::Text("the index is ".to_owned()),
            PrintPart::Value(index.clone()),
            PrintPart::Text(" but the length is ".to_owned()),
            PrintPart::Value(len.clone()),
        ];
        let negative = self.binary_temp(BinaryOp::Lt, index.clone(), int_const(0), Ty::Bool, span);
        self.panic_if(negative, PanicKind::IndexOutOfBounds, message.clone(), span);
        let past = if allow_end {
            BinaryOp::Gt
        } else {
            BinaryOp::Ge
        };
        let too_large = self.binary_temp(past, index, len, Ty::Bool, span);
        self.panic_if(too_large, PanicKind::IndexOutOfBounds, message, span);
    }

    /// Panics if the `bool` condition is true.
    fn panic_if(
        &mut self,
        condition: Operand,
        kind: PanicKind,
        message: Vec<PrintPart>,
        span: Span,
    ) {
        let panic_block = self.new_block();
        let next = self.new_block();
        self.branch(condition, panic_block, next);
        self.switch_to(panic_block);
        self.terminate(Terminator::Panic {
            kind,
            message,
            span,
        });
        self.switch_to(next);
    }

    /// The value of a part of a global or of a captured value, read now into a temporary.
    fn read_global_part(&mut self, place: Place, ty: Ty, span: Span) -> Operand {
        let value = if self.is_copy(ty) {
            Rvalue::Use(Operand::Copy { place, span })
        } else {
            Rvalue::Clone(place)
        };
        // A read of the temporary, which is destroyed with the statement's others.
        self.assign_temp(ty, value, span)
    }

    /// Reports a change to `place` (an assignment, a move or a `mut` borrow) while a `:match`
    /// arm or a `:foreach` loop borrows it.
    fn check_not_borrowed(&mut self, place: &Place, span: Span) {
        let Some(&(_, borrowed_at, borrower)) = self
            .match_borrows
            .iter()
            .find(|(borrowed, ..)| borrowed.overlaps(place))
        else {
            return;
        };
        let (message, label, help) = match borrower {
            Borrower::Match => (
                "cannot change or move this value while a `:match` arm borrows it",
                "the value matched, borrowed by the arm's names",
                "copy what the arm needs with `->clone` before changing the value",
            ),
            Borrower::Foreach => (
                "cannot change or move a collection while `:foreach` iterates over it",
                "the collection iterated over",
                "collect the changes in another variable and apply them after the loop",
            ),
        };
        self.error(
            Diagnostic::error(codes::BORROWED_BY_MATCH, message, span)
                .with_secondary(borrowed_at, label)
                .with_help(help),
        );
    }

    /// The source form of a variable or a field of one, such as `$p->a->b`, for messages.
    fn source_text(&self, expr: hir::ExprId) -> Option<String> {
        match &self.hir.exprs[expr] {
            hir::Expr::Local(local) => Some(format!("${}", self.hir.locals[*local].name.value)),
            hir::Expr::Global(global) => {
                Some(format!("${}", self.module.globals[*global].name.value))
            }
            hir::Expr::Field { base, field } => {
                Some(format!("{}->{}", self.source_text(*base)?, field.value))
            }
            _ => None,
        }
    }

    /// The variable at the root of a place expression: `$p` in `$p->a->b`.
    fn root_expr(&self, expr: hir::ExprId) -> hir::ExprId {
        match self.hir.exprs[expr] {
            hir::Expr::Field { base, .. } | hir::Expr::Index { base, .. } => self.root_expr(base),
            _ => expr,
        }
    }

    /// A place holding the value of `expr`, for borrowing it: the variable (or its field)
    /// itself, or a temporary holding the computed value.
    fn borrow_place(&mut self, expr: hir::ExprId) -> Place {
        let span = self.hir.expr_span(expr);
        let value = match self.place_of(expr) {
            Some(place) if matches!(place.root(), PlaceRoot::Local(_)) && !self.eager_reads => {
                return place;
            }
            Some(place) if matches!(place.root(), PlaceRoot::Local(_)) => {
                let ty = self.expr_ty(expr);
                self.take_now(Operand::Copy { place, span }, ty)
            }
            Some(place) => self.read_global_part(place, self.expr_ty(expr), span),
            None => self.lower_read(expr),
        };
        if let Operand::Copy {
            place: Place::Local(local),
            ..
        }
        | Operand::Move {
            place: Place::Local(local),
            ..
        } = value
            && self.locals[local].user.is_none()
        {
            return Place::Local(local);
        }
        let ty = self.expr_ty(expr);
        let value = self.owned_copy(value, ty, span);
        let temp = self.temp(ty);
        self.assign(Place::Local(temp), Rvalue::Use(value), span);
        Place::Local(temp)
    }

    /// Converts a read operand into one that can be owned: copies of `Copy` values and
    /// constants as they are, and a clone of anything else.
    fn owned_copy(&mut self, value: Operand, ty: Ty, span: Span) -> Operand {
        match value {
            Operand::Copy { place, .. } if !self.is_copy(ty) => {
                let clone = self.assign_temp(ty, Rvalue::Clone(place), span);
                to_move(clone)
            }
            value => value,
        }
    }

    // ----- Expressions -------------------------------------------------------------------

    /// Evaluates an expression whose value is consumed: moved if it is not `Copy`.
    /// Evaluates an expression whose value is consumed: moved if it is not `Copy`.
    fn lower_consume(&mut self, expr: hir::ExprId) -> Operand {
        let value = match self.wrapped(expr) {
            Some(option) => {
                let value = self.lower_consume_own(expr);
                return self.wrap_some(option, value, self.hir.expr_span(expr));
            }
            None => self.lower_consume_own(expr),
        };
        self.take_now(value, self.expr_ty(expr))
    }

    /// Evaluates an expression into an operand that is read without taking ownership.
    pub(crate) fn lower_read(&mut self, expr: hir::ExprId) -> Operand {
        // The option of a wrapped value owns a copy of the value.
        if let Some(option) = self.wrapped(expr) {
            let span = self.hir.expr_span(expr);
            let value = self.lower_read_own(expr);
            let value = self.owned_copy(value, self.own_ty(expr), span);
            return self.wrap_some(option, value, span);
        }
        let value = self.lower_read_own(expr);
        self.take_now(value, self.expr_ty(expr))
    }

    /// In eager mode, takes the value of a variable read by `value` now, into a temporary.
    fn take_now(&mut self, value: Operand, ty: Ty) -> Operand {
        let (Operand::Copy { place, span } | Operand::Move { place, span }) = &value else {
            return value;
        };
        if !self.eager_reads || !self.is_variable(place) {
            return value;
        }
        let (place, span) = (place.clone(), *span);
        let moved = matches!(value, Operand::Move { .. });
        let taken = if moved || self.is_copy(ty) {
            Rvalue::Use(value)
        } else {
            Rvalue::Clone(place)
        };
        let temp = self.assign_temp(ty, taken, span);
        // A read stays a read of the temporary, which is dropped with the expression.
        if moved { to_move(temp) } else { temp }
    }

    /// Returns true if `place` is part of a variable written in the source, which code may
    /// change, rather than of a temporary.
    fn is_variable(&self, place: &Place) -> bool {
        match place.root() {
            PlaceRoot::Local(local) => self.locals[local].user.is_some(),
            PlaceRoot::Global(_) => true,
            // Captured values never change.
            PlaceRoot::Capture(_) => false,
        }
    }

    /// Runs `f` with reads taken eagerly if `eager`.
    fn with_eager<T>(&mut self, eager: bool, f: impl FnOnce(&mut Self) -> T) -> T {
        let outer = std::mem::replace(&mut self.eager_reads, eager);
        let result = f(self);
        self.eager_reads = outer;
        result
    }

    /// For each expression, whether a later one may change a variable: then the earlier one
    /// is evaluated eagerly, so that operands are evaluated left to right.
    fn later_changes(&self, exprs: &[hir::ExprId]) -> Vec<bool> {
        let mut later = vec![false; exprs.len()];
        let mut changes = false;
        for (index, &expr) in exprs.iter().enumerate().rev() {
            later[index] = changes;
            changes |= self.may_change(expr);
        }
        later
    }

    /// Whether evaluating `expr` may change a variable of this function: through a call
    /// with a `mut` argument, or a method that changes its receiver.
    fn may_change(&self, expr: hir::ExprId) -> bool {
        let module = self.module;
        let has_mut = |id: hir::FnId| {
            module.functions[id]
                .params
                .iter()
                .any(|p| p.convention == Convention::Mut)
        };
        let changes = match &self.hir.exprs[expr] {
            hir::Expr::Call {
                callee: Callee::Fn(id),
                ..
            } => has_mut(*id),
            hir::Expr::MethodCall { .. } => match self.types.methods.get(expr) {
                Some(&Method::Fn(id)) => has_mut(id),
                Some(
                    Method::Push
                    | Method::Pop
                    | Method::Swap
                    | Method::Insert
                    | Method::Remove
                    | Method::Clear
                    | Method::Take
                    | Method::Write,
                ) => true,
                _ => false,
            },
            _ => false,
        };
        changes
            || self.hir.exprs[expr]
                .children()
                .into_iter()
                .any(|child| self.may_change(child))
    }

    /// `[some value]`, for a value implicitly wrapped in an option (spec section 5.4).
    fn wrap_some(&mut self, option: Ty, value: Operand, span: Span) -> Operand {
        let wrapped = self.assign_temp(
            option,
            Rvalue::Variant {
                ty: option,
                variant: 1,
                fields: vec![value],
            },
            span,
        );
        to_move(wrapped)
    }

    /// The error for a move out of a borrowed local: a parameter, a `:match` name or a
    /// `:foreach` element.
    fn borrowed_move_error(&self, local: hir::LocalId, span: Span) -> Diagnostic {
        let decl = &self.hir.locals[local];
        let name = &decl.name.value;
        let (reason, help) = match decl.kind {
            hir::LocalKind::Element { .. } => (
                "it is an element of the collection iterated over",
                format!("use a copy: `[${name}->clone]`"),
            ),
            hir::LocalKind::Binding => (
                "it borrows part of the value matched",
                format!(
                    "use a copy: `[${name}->clone]`; names bound by matching a temporary value own their parts"
                ),
            ),
            _ => (
                "it is a borrowed parameter",
                format!("pass a copy with `[${name}->clone]`, or declare the parameter `owned`"),
            ),
        };
        Diagnostic::error(
            codes::MOVE_OUT_OF_BORROW,
            format!("cannot move `{name}` out: {reason}"),
            span,
        )
        .with_help(help)
    }

    /// The error for a move out of a part of a value: a field, an element or a boxed value.
    fn part_move_error(&self, expr: hir::ExprId, span: Span) -> Diagnostic {
        let text = self.source_text(expr);
        let (message, help) = match &self.hir.exprs[expr] {
            hir::Expr::Index { .. } => (
                "cannot move an element out of its collection".to_owned(),
                "take it out with `[$list->remove $i]` or `[$map->remove $k]`, or use a copy made with `->clone`".to_owned(),
            ),
            hir::Expr::Field { field, .. } => match self.types.projections.get(expr) {
                Some(Projection::BoxValue) => (
                    "cannot move the value out of its box".to_owned(),
                    match &text {
                        Some(text) => format!(
                            "take it out with `[{}->unbox]`, or use a copy: `[{text}->clone]`",
                            text.trim_end_matches("->value")
                        ),
                        None => "take it out with `->unbox`".to_owned(),
                    },
                ),
                _ => (
                    format!("cannot move the field `{}` out of its struct", field.value),
                    match &text {
                        Some(text) => format!("use a copy: `[{text}->clone]`"),
                        None => "use a copy made with `->clone`".to_owned(),
                    },
                ),
            },
            _ => unreachable!("not a part of a value"),
        };
        Diagnostic::error(codes::MOVE_OUT_OF_BORROW, message, span).with_help(help)
    }

    /// Evaluates an expression whose own value (before any wrapping) is consumed.
    fn lower_consume_own(&mut self, expr: hir::ExprId) -> Operand {
        let ty = self.own_ty(expr);
        if self.is_copy(ty) {
            return self.lower_read_own(expr);
        }
        let span = self.hir.expr_span(expr);
        // A string taken from a variable, a field or an element is copied, not moved: copies
        // share the text until one of them changes, so they are cheap (spec section 12.1).
        if ty == Ty::String {
            match self.hir.exprs[expr] {
                // Read into a copy already, as globals are.
                hir::Expr::Global(_) => return to_move(self.lower_read_own(expr)),
                hir::Expr::Local(_) | hir::Expr::Index { .. } | hir::Expr::Field { .. } => {
                    let value = self.lower_read_own(expr);
                    return self.owned_copy(value, ty, span);
                }
                _ => {}
            }
        }
        match self.hir.exprs[expr] {
            hir::Expr::Local(local) if self.capture_map.get(local).is_some() => {
                let place = self.local_place(local);
                let name = &self.hir.locals[local].name.value;
                self.error(
                    Diagnostic::error(
                        codes::MOVE_OUT_OF_BORROW,
                        format!("cannot move `{name}` out: it is captured by this function value"),
                        span,
                    )
                    .with_help(format!(
                        "copies of a function value share what it captured; use a copy: `[${name}->clone]`"
                    )),
                );
                Operand::Copy { place, span }
            }
            hir::Expr::Local(local) => {
                let id = self.local_map[local];
                if let LocalMode::Ref { .. } = self.locals[id].mode {
                    let diagnostic = self.borrowed_move_error(local, span);
                    self.error(diagnostic);
                    return Operand::Copy {
                        place: Place::Local(id),
                        span,
                    };
                }
                self.check_not_borrowed(&Place::Local(id), span);
                Operand::Move {
                    place: Place::Local(id),
                    span,
                }
            }
            hir::Expr::Index { .. } | hir::Expr::Field { .. } => {
                let diagnostic = self.part_move_error(expr, span);
                self.error(diagnostic);
                self.lower_read_own(expr)
            }
            hir::Expr::Global(global) => {
                // Moving out of a global would leave it empty for the rest of the program.
                let name = &self.module.globals[global].name.value;
                self.error(
                    Diagnostic::error(
                        codes::MOVE_OUT_OF_BORROW,
                        format!("cannot move the global `{name}` out"),
                        span,
                    )
                    .with_help(format!("use a copy: `[${name}->clone]`")),
                );
                Operand::Copy {
                    place: Place::Global(global),
                    span,
                }
            }
            _ => {
                let value = self.lower_read_own(expr);
                if let Operand::Copy {
                    place: Place::Local(local),
                    ..
                } = value
                    && self.locals[local].user.is_none()
                {
                    return to_move(value);
                }
                value
            }
        }
    }

    /// Like [`Builder::lower_read`], for the expression's own value (before any wrapping).
    fn lower_read_own(&mut self, expr: hir::ExprId) -> Operand {
        let span = self.hir.expr_span(expr);
        let ty = self.own_ty(expr);
        match &self.hir.exprs[expr] {
            hir::Expr::Missing => Operand::Const(Value::Nothing),
            hir::Expr::Literal(literal) => Operand::Const(literal_value(literal, ty)),
            hir::Expr::String(_) => {
                let parts = self.string_parts(expr);
                if parts.is_empty() {
                    return Operand::Const(Value::Str("".into()));
                }
                if let [PrintPart::Text(text)] = parts.as_slice() {
                    return Operand::Const(Value::Str(text.as_str().into()));
                }
                self.assign_temp(Ty::String, Rvalue::Interpolate(parts), span)
            }
            hir::Expr::Local(local) => Operand::Copy {
                place: self.local_place(*local),
                span,
            },
            &hir::Expr::Closure(id) => self.lower_closure_value(id, ty, span),
            &hir::Expr::FnRef(id) => {
                let func = self
                    .instances
                    .as_mut()
                    .expect("only function bodies use function values")
                    .request_adapter(id);
                let value = Rvalue::Closure {
                    ty,
                    func,
                    captures: Vec::new(),
                };
                self.assign_temp(ty, value, span)
            }
            hir::Expr::CallValue { callee, args } => {
                let (callee, args) = (*callee, args.clone());
                self.lower_value_call(callee, &args, ty, span)
            }
            hir::Expr::Const(id) => Operand::Const(self.consts.const_value(*id)),
            // A global is read now, not when the operand is used: a call evaluated in between
            // could change it. Values that are not `Copy` are cloned for the same reason.
            hir::Expr::Global(global) => {
                let place = Place::Global(*global);
                let value = if self.is_copy(ty) {
                    Rvalue::Use(Operand::Copy { place, span })
                } else {
                    Rvalue::Clone(place)
                };
                self.assign_temp(ty, value, span)
            }
            hir::Expr::Call { callee, args, .. } => self.lower_call(expr, *callee, args, ty, span),
            hir::Expr::MethodCall { receiver, .. } => {
                self.lower_method_call(expr, *receiver, ty, span)
            }
            &hir::Expr::Field { base, .. } => self.lower_field(expr, base, ty, span),
            &hir::Expr::Index { base, index } => self.lower_index(expr, base, index, ty, span),
            hir::Expr::StructLit { .. }
            | hir::Expr::Collection { .. }
            | hir::Expr::Variant { .. }
            | hir::Expr::Some(_)
            | hir::Expr::None
            | hir::Expr::BoxNew(_) => self.lower_constructor(expr, ty, span),
            &hir::Expr::Unary { operand, .. } | &hir::Expr::Binary { lhs: operand, .. }
                if let Some(&Method::Fn(function)) = self.types.methods.get(expr) =>
            {
                let operands = match self.hir.exprs[expr] {
                    hir::Expr::Binary { lhs, rhs, .. } => vec![lhs, rhs],
                    _ => vec![operand],
                };
                self.lower_operator_call(expr, function, &operands, ty, span)
            }
            &hir::Expr::Unary { op, operand } => {
                let operand = self.lower_read(operand);
                let op = match op {
                    hir::UnaryOp::Neg => UnaryOp::Neg,
                    hir::UnaryOp::Not => UnaryOp::Not,
                    hir::UnaryOp::BitNot => UnaryOp::BitNot,
                };
                self.assign_temp(ty, Rvalue::Unary { op, operand }, span)
            }
            &hir::Expr::Binary { op, lhs, rhs, .. } => self.lower_binary(op, lhs, rhs, ty, span),
            &hir::Expr::Cast { expr: inner, .. } => {
                let operand = self.lower_read(inner);
                self.assign_temp(
                    ty,
                    Rvalue::Cast {
                        kind: CastKind::As,
                        operand,
                        to: ty,
                    },
                    span,
                )
            }
            &hir::Expr::Raise { value, source } => self.lower_raise(value, source, span),
            hir::Expr::Return(_) | hir::Expr::Break | hir::Expr::Continue => {
                self.lower_jump(expr, span)
            }
        }
    }

    /// `:return`, `:break` or `:continue`: drops what goes out of scope, then jumps.
    /// The place of a variable of the source: a local slot, or a value captured by the
    /// function value whose body this is.
    fn local_place(&self, local: hir::LocalId) -> Place {
        match self.capture_map.get(local) {
            Some(&index) => Place::Capture(index),
            None => Place::Local(self.local_map[local]),
        }
    }

    /// `[:fn ...]`: a new function value, which captures the variables its body uses.
    fn lower_closure_value(&mut self, id: hir::FnId, ty: Ty, span: Span) -> Operand {
        let captures: Vec<hir::LocalId> = self.module.functions[id]
            .captures
            .iter()
            .map(|capture| capture.outer)
            .collect();
        let captures = captures
            .into_iter()
            .map(|outer| self.capture_operand(outer, span))
            .collect();
        let func = self
            .instances
            .as_mut()
            .expect("only function bodies create function values")
            .request(id, self.subst.clone(), span);
        self.assign_temp(ty, Rvalue::Closure { ty, func, captures }, span)
    }

    /// The value of variable `outer` captured by a new function value: copied if `Copy`,
    /// moved otherwise (a value this body captured itself is cloned: it is shared).
    fn capture_operand(&mut self, outer: hir::LocalId, span: Span) -> Operand {
        let ty = self.local_ty(outer);
        let place = self.local_place(outer);
        if self.is_copy(ty) {
            return Operand::Copy { place, span };
        }
        match place {
            Place::Capture(_) => to_move(self.assign_temp(ty, Rvalue::Clone(place), span)),
            Place::Local(id) if matches!(self.locals[id].mode, LocalMode::Ref { .. }) => {
                let diagnostic = self.borrowed_move_error(outer, span);
                self.error(diagnostic);
                Operand::Copy { place, span }
            }
            _ => {
                self.check_not_borrowed(&place, span);
                Operand::Move { place, span }
            }
        }
    }

    /// `[$f args]`: a call of a function value, which borrows it and its arguments.
    fn lower_value_call(
        &mut self,
        callee: hir::ExprId,
        args: &[hir::ExprId],
        ty: Ty,
        span: Span,
    ) -> Operand {
        let Ty::Fn(function) = self.own_ty(callee) else {
            return Operand::Const(Value::Nothing);
        };
        let evaluated: Vec<hir::ExprId> = std::iter::once(callee)
            .chain(args.iter().copied())
            .collect();
        let mut eager = self.later_changes(&evaluated).into_iter();
        let callee = self.with_eager(eager.next().unwrap_or(false), |b| b.borrow_place(callee));
        let mut call_args = Vec::with_capacity(args.len());
        for (&arg, &param_ty) in args.iter().zip(function.params.iter()) {
            let lowered = self.with_eager(eager.next().unwrap_or(false), |b| {
                b.lower_arg(arg, Convention::Read, param_ty)
            });
            call_args.push(lowered);
        }
        self.emit_call_to(
            CallTarget::Value(callee),
            function.raises,
            call_args,
            ty,
            span,
        )
    }

    /// `[$value->field args]`: a call of the function value in a field of `receiver`.
    fn lower_field_call(
        &mut self,
        expr: hir::ExprId,
        receiver: hir::ExprId,
        index: u32,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let args: Vec<hir::ExprId> = match &self.hir.exprs[expr] {
            hir::Expr::MethodCall { args, .. } => args.iter().map(|a| a.value).collect(),
            _ => Vec::new(),
        };
        let args = args.as_slice();
        let struct_ty = self.own_ty(receiver);
        let Ty::Fn(function) = self.structs.field_ty(struct_ty, index) else {
            return Operand::Const(Value::Nothing);
        };
        let evaluated: Vec<hir::ExprId> = std::iter::once(receiver)
            .chain(args.iter().copied())
            .collect();
        let mut eager = self.later_changes(&evaluated).into_iter();
        let base = self.with_eager(eager.next().unwrap_or(false), |b| b.borrow_place(receiver));
        let callee = base.field(index);
        let mut call_args = Vec::with_capacity(args.len());
        for (&arg, &param_ty) in args.iter().zip(function.params.iter()) {
            let lowered = self.with_eager(eager.next().unwrap_or(false), |b| {
                b.lower_arg(arg, Convention::Read, param_ty)
            });
            call_args.push(lowered);
        }
        self.emit_call_to(
            CallTarget::Value(callee),
            function.raises,
            call_args,
            ty,
            span,
        )
    }

    /// The type of errors.
    fn error_ty(&self) -> Ty {
        self.module
            .error_type
            .map_or(Ty::Error, |id| self.module.types[id].ty)
    }

    /// Continues with the error in `error`, at the point where it is raised: destroys the
    /// temporaries and the variables of the scopes it leaves, then goes to the innermost
    /// `:onerror` handler, or leaves the function with the error.
    fn raise_from(&mut self, error: LocalId, span: Span) {
        let (scope_depth, temps_depth) = self
            .handlers
            .last()
            .map_or((0, 0), |h| (h.scope_depth, h.temps_depth));
        let temps: Vec<LocalId> = self.temps[temps_depth.min(self.temps.len())..].to_vec();
        for temp in temps.into_iter().rev() {
            self.drop_local(temp);
        }
        self.drop_scopes_from(scope_depth);
        let error = Operand::Move {
            place: Place::Local(error),
            span,
        };
        match self.handlers.last() {
            Some(handler) => {
                let (target, block) = (handler.error, handler.block);
                self.assign(Place::Local(target), Rvalue::Use(error), span);
                self.goto(block);
            }
            None => self.terminate(Terminator::Raise(error)),
        }
    }

    /// Adds the location of the call at `span` to the trace of the error in `error`, which
    /// came out of that call: the calls an error passes through, innermost first.
    fn record_trace(&mut self, error: LocalId, span: Span) {
        let Some(sources) = self.locate else {
            return;
        };
        let error_ty = self.error_ty();
        let field = self
            .structs
            .adt(error_ty)
            .and_then(|info| info.fields.iter().position(|(name, _)| name == "trace"));
        let Some(field) = field else {
            return;
        };
        let location = sources.locate(span);
        let text = format!(
            "{}:{}:{}",
            sources.file(location.file).name,
            location.line,
            location.column
        );
        self.push(Statement::ListPush {
            list: Place::Field(
                Box::new(Place::Local(error)),
                u32::try_from(field).expect("few fields"),
            ),
            value: Operand::Const(Value::Str(text.into())),
        });
    }

    /// `:error value` or `:error value source=$cause`: raises an error made from a message
    /// at this location, or the error given.
    fn lower_raise(
        &mut self,
        value: hir::ExprId,
        source: Option<hir::ExprId>,
        span: Span,
    ) -> Operand {
        let error_ty = self.error_ty();
        let error_value = if self.own_ty(value) == error_ty {
            self.lower_consume(value)
        } else {
            let message = self.lower_consume(value);
            let source = match source {
                Some(source) => {
                    let cause = self.lower_consume(source);
                    let boxed = self.assign_temp(Ty::boxed(error_ty), Rvalue::BoxNew(cause), span);
                    let option = Ty::option(Ty::boxed(error_ty));
                    let some = Rvalue::Variant {
                        ty: option,
                        variant: 1,
                        fields: vec![to_move(boxed)],
                    };
                    to_move(self.assign_temp(option, some, span))
                }
                None => to_move(self.assign_temp(
                    Ty::option(Ty::boxed(error_ty)),
                    Rvalue::Variant {
                        ty: Ty::option(Ty::boxed(error_ty)),
                        variant: 0,
                        fields: Vec::new(),
                    },
                    span,
                )),
            };
            let (file, line, column) = self.locate.map_or((String::new(), 0, 0), |sources| {
                let location = sources.locate(span);
                (
                    sources.file(location.file).name.clone(),
                    location.line,
                    location.column,
                )
            });
            let u32_const = |value: u32| {
                Operand::Const(Value::Int {
                    value: i128::from(value),
                    ty: IntTy::U32,
                })
            };
            let file = Operand::Const(Value::Str(file.into()));
            let trace_ty = Ty::list(Ty::String);
            let trace = to_move(self.assign_temp(
                trace_ty,
                Rvalue::List {
                    ty: trace_ty,
                    elements: Vec::new(),
                },
                span,
            ));
            let fields = vec![
                message,
                source,
                file,
                u32_const(line),
                u32_const(column),
                trace,
            ];
            to_move(self.assign_temp(
                error_ty,
                Rvalue::Struct {
                    ty: error_ty,
                    fields,
                },
                span,
            ))
        };
        let error = self.alloc_local(error_ty, LocalMode::Value, None);
        self.assign(Place::Local(error), Rvalue::Use(error_value), span);
        self.raise_from(error, span);
        Operand::Const(Value::Nothing)
    }

    /// `:onerror e in={...} do={...}`
    fn lower_onerror(&mut self, error: hir::LocalId, body: &hir::Block, handler: &hir::Block) {
        let decl = &self.hir.locals[error];
        let local = self.alloc_local(
            self.error_ty(),
            LocalMode::Value,
            Some(UserVariable {
                name: decl.name.value.clone(),
                span: decl.name.span,
                declared_without_value: false,
            }),
        );
        self.local_map.insert(error, local);
        let handler_block = self.new_block();
        self.handlers.push(Handler {
            error: local,
            block: handler_block,
            scope_depth: self.scopes.len(),
            temps_depth: self.temps.len(),
        });
        self.lower_block(body);
        self.handlers.pop();
        let after = self.new_block();
        if self.current.is_some() {
            self.goto(after);
        }
        self.switch_to(handler_block);
        self.push_scope();
        self.scopes.last_mut().expect("a scope is open").push(local);
        self.lower_block(handler);
        self.pop_scope();
        if self.current.is_some() {
            self.goto(after);
        }
        self.switch_to(after);
    }

    fn lower_jump(&mut self, expr: hir::ExprId, span: Span) -> Operand {
        if let hir::Expr::Return(value) = self.hir.exprs[expr] {
            let return_local = self.return_local.expect("set before lowering");
            if let Some(value) = value {
                let value = self.lower_consume(value);
                self.assign(Place::Local(return_local), Rvalue::Use(value), span);
            }
            self.drop_all_temps();
            self.drop_scopes_from(0);
            self.terminate(Terminator::Return);
        } else if let Some(targets) = self.loops.last() {
            let is_break = matches!(self.hir.exprs[expr], hir::Expr::Break);
            let (target, depth) = if is_break {
                (targets.break_block, targets.scope_depth)
            } else {
                (targets.continue_block, targets.scope_depth)
            };
            self.drop_all_temps();
            self.drop_scopes_from(depth);
            self.goto(target);
        }
        Operand::Const(Value::Nothing)
    }

    /// A new value: a struct, an enum variant, an option or a box.
    fn lower_constructor(&mut self, expr: hir::ExprId, ty: Ty, span: Span) -> Operand {
        match &self.hir.exprs[expr] {
            hir::Expr::StructLit { strukt, fields, .. } => {
                let (strukt, fields) = (*strukt, fields.clone());
                let given: Vec<hir::ExprId> = fields.iter().flatten().copied().collect();
                let mut given = self.consume_all(&given).into_iter();
                let mut values = Vec::with_capacity(fields.len());
                for (index, field) in fields.into_iter().enumerate() {
                    values.push(match field {
                        Some(_) => given.next().expect("one operand per given field"),
                        None => Operand::Const(self.consts.field_default(strukt, index)),
                    });
                }
                self.assign_temp(ty, Rvalue::Struct { ty, fields: values }, span)
            }
            &hir::Expr::Variant { variant, .. } => {
                let args = self.types.call_args.get(expr).cloned().unwrap_or_default();
                // Evaluated as written, then placed in field order.
                let order = self.source_order(expr, &args);
                let values: Vec<hir::ExprId> = order.iter().map(|&(_, value)| value).collect();
                let mut fields = vec![Operand::Const(Value::Nothing); args.len()];
                for ((position, _), operand) in order.iter().zip(self.consume_all(&values)) {
                    fields[*position] = operand;
                }
                let variant = u32::try_from(variant).expect("enums have few variants");
                self.assign_temp(
                    ty,
                    Rvalue::Variant {
                        ty,
                        variant,
                        fields,
                    },
                    span,
                )
            }
            &hir::Expr::Some(value) => {
                let value = self.lower_consume(value);
                self.assign_temp(
                    ty,
                    Rvalue::Variant {
                        ty,
                        variant: 1,
                        fields: vec![value],
                    },
                    span,
                )
            }
            hir::Expr::None => self.assign_temp(
                ty,
                Rvalue::Variant {
                    ty,
                    variant: 0,
                    fields: Vec::new(),
                },
                span,
            ),
            &hir::Expr::BoxNew(value) => {
                let value = self.lower_consume(value);
                self.assign_temp(ty, Rvalue::BoxNew(value), span)
            }
            hir::Expr::Collection { elements, .. } => {
                let elements = elements.clone();
                let exprs = self.hir.exprs[expr].children();
                let mut operands = self.consume_all(&exprs).into_iter();
                let mut next = || operands.next().expect("one operand per element");
                let rvalue = if let Ty::List(_) = ty {
                    Rvalue::List {
                        ty,
                        elements: elements.iter().map(|_| next()).collect(),
                    }
                } else {
                    let entries = elements
                        .iter()
                        .map(|element| match element {
                            hir::Element::Value(_) => (next(), Operand::Const(Value::Nothing)),
                            hir::Element::Entry(..) => (next(), next()),
                        })
                        .collect();
                    Rvalue::Map { ty, entries }
                };
                self.assign_temp(ty, rvalue, span)
            }
            _ => unreachable!("not a constructor"),
        }
    }

    /// Evaluates expressions in order, each consumed; a value read from a variable is taken
    /// at once when a later expression may change the variable.
    fn consume_all(&mut self, exprs: &[hir::ExprId]) -> Vec<Operand> {
        let eager = self.later_changes(exprs);
        exprs
            .iter()
            .zip(eager)
            .map(|(&expr, eager)| self.with_eager(eager, |b| b.lower_consume(expr)))
            .collect()
    }

    /// Reads a field, or the value of a box: in place for a part of a local, otherwise from
    /// a temporary holding the part or the whole value.
    fn lower_field(&mut self, expr: hir::ExprId, base: hir::ExprId, ty: Ty, span: Span) -> Operand {
        if let Some(place) = self.place_of(expr) {
            if let PlaceRoot::Local(_) = place.root() {
                return Operand::Copy { place, span };
            }
            return self.read_global_part(place, ty, span);
        }
        let base_value = self.lower_read(base);
        match base_value {
            Operand::Copy { place, .. } | Operand::Move { place, .. } => {
                match self.project(expr, &place) {
                    Some(place) => Operand::Copy { place, span },
                    None => Operand::Const(Value::Nothing),
                }
            }
            Operand::Const(Value::Struct { fields, .. }) => {
                let Some(&Projection::Field(index)) = self.types.projections.get(expr) else {
                    return Operand::Const(Value::Nothing);
                };
                Operand::Const(fields.get(index).cloned().unwrap_or(Value::Nothing))
            }
            Operand::Const(_) => Operand::Const(Value::Nothing),
        }
    }

    /// Reads an element of a list or a value of a map.
    fn lower_index(
        &mut self,
        expr: hir::ExprId,
        base: hir::ExprId,
        index: hir::ExprId,
        ty: Ty,
        span: Span,
    ) -> Operand {
        if let Some(place) = self.place_of(expr) {
            if let PlaceRoot::Local(_) = place.root() {
                return Operand::Copy { place, span };
            }
            return self.read_global_part(place, ty, span);
        }
        // The collection is a temporary, which lives until the end of the statement.
        let base_place = match self.lower_read(base) {
            Operand::Copy { place, .. } | Operand::Move { place, .. } => place,
            Operand::Const(value) => {
                let collection = self.own_ty(base);
                let temp = self.temp(collection);
                self.assign(Place::Local(temp), Rvalue::Use(Operand::Const(value)), span);
                Place::Local(temp)
            }
        };
        let place = self.element_place(base_place, self.own_ty(base), index);
        Operand::Copy { place, span }
    }

    fn lower_binary(
        &mut self,
        op: hir::BinaryOp,
        lhs: hir::ExprId,
        rhs: hir::ExprId,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let op = match op {
            hir::BinaryOp::And | hir::BinaryOp::Or => {
                return self.lower_short_circuit(op == hir::BinaryOp::And, lhs, rhs, span);
            }
            hir::BinaryOp::Add => BinaryOp::Add,
            hir::BinaryOp::Sub => BinaryOp::Sub,
            hir::BinaryOp::Mul => BinaryOp::Mul,
            hir::BinaryOp::Div => BinaryOp::Div,
            hir::BinaryOp::Rem => BinaryOp::Rem,
            hir::BinaryOp::Shl => BinaryOp::Shl,
            hir::BinaryOp::Shr => BinaryOp::Shr,
            hir::BinaryOp::BitAnd => BinaryOp::BitAnd,
            hir::BinaryOp::BitOr => BinaryOp::BitOr,
            hir::BinaryOp::BitXor => BinaryOp::BitXor,
            hir::BinaryOp::Concat => BinaryOp::Concat,
            hir::BinaryOp::Eq => BinaryOp::Eq,
            hir::BinaryOp::Ne => BinaryOp::Ne,
            hir::BinaryOp::Lt => BinaryOp::Lt,
            hir::BinaryOp::Le => BinaryOp::Le,
            hir::BinaryOp::Gt => BinaryOp::Gt,
            hir::BinaryOp::Ge => BinaryOp::Ge,
            hir::BinaryOp::In => BinaryOp::Contains,
        };
        if op == BinaryOp::Contains
            && let Ty::List(_) | Ty::Set(_) | Ty::Map(_) = self.expr_ty(rhs)
        {
            let value = self.with_eager(self.may_change(rhs), |b| b.lower_read(lhs));
            let collection = self.borrow_place(rhs);
            return self.assign_temp(Ty::Bool, Rvalue::Contains { collection, value }, span);
        }
        let lhs = self.with_eager(self.may_change(rhs), |b| b.lower_read(lhs));
        let rhs = self.lower_read(rhs);
        self.binary_temp(op, lhs, rhs, ty, span)
    }

    /// `lhs and rhs` evaluates `rhs` only if `lhs` is true; `lhs or rhs` only if it is false.
    fn lower_short_circuit(
        &mut self,
        is_and: bool,
        lhs: hir::ExprId,
        rhs: hir::ExprId,
        span: Span,
    ) -> Operand {
        let result = self.temp(Ty::Bool);
        let lhs = self.lower_read(lhs);
        let evaluate_rhs = self.new_block();
        let shortcut = self.new_block();
        let join = self.new_block();
        let (then_block, else_block) = if is_and {
            (evaluate_rhs, shortcut)
        } else {
            (shortcut, evaluate_rhs)
        };
        self.branch(lhs, then_block, else_block);
        self.switch_to(shortcut);
        self.assign(
            Place::Local(result),
            Rvalue::Use(Operand::Const(Value::Bool(!is_and))),
            span,
        );
        self.goto(join);
        self.switch_to(evaluate_rhs);
        let rhs = self.lower_read(rhs);
        self.assign(Place::Local(result), Rvalue::Use(rhs), span);
        if self.current.is_some() {
            self.goto(join);
        }
        self.switch_to(join);
        Operand::Copy {
            place: Place::Local(result),
            span,
        }
    }

    fn lower_method_call(
        &mut self,
        expr: hir::ExprId,
        receiver: hir::ExprId,
        ty: Ty,
        span: Span,
    ) -> Operand {
        match self.types.methods.get(expr) {
            Some(Method::Clone) => {
                let place = self.borrow_place(receiver);
                self.assign_temp(ty, Rvalue::Clone(place), span)
            }
            Some(&Method::CallField(index)) => {
                self.lower_field_call(expr, receiver, index, ty, span)
            }
            Some(Method::Write) => {
                let target = self.changed_receiver(expr, receiver);
                let value = match &self.hir.exprs[expr] {
                    hir::Expr::MethodCall { args, .. } => args.first().map(|arg| arg.value),
                    _ => None,
                };
                let parts = value.map(|v| self.print_parts(v)).unwrap_or_default();
                self.push(Statement::Append { target, parts });
                Operand::Const(Value::Nothing)
            }
            Some(&Method::Fn(id)) => self.lower_fn_call(expr, id, ty, span, Some(receiver)),
            Some(&(Method::IsSome | Method::IsNone)) => {
                let variant = u32::from(self.types.methods[expr] == Method::IsSome);
                let place = self.borrow_place(receiver);
                let discriminant = self.discriminant(place, span);
                self.binary_temp(
                    BinaryOp::Eq,
                    discriminant,
                    variant_const(variant),
                    Ty::Bool,
                    span,
                )
            }
            Some(Method::Unwrap) => {
                let option = self.consumed_temp(receiver);
                let discriminant = self.discriminant(Place::Local(option), span);
                let is_some =
                    self.binary_temp(BinaryOp::Eq, discriminant, variant_const(1), Ty::Bool, span);
                let some_block = self.new_block();
                let none_block = self.new_block();
                self.branch(is_some, some_block, none_block);
                self.switch_to(none_block);
                self.terminate(Terminator::Panic {
                    kind: PanicKind::UnwrapNone,
                    message: Vec::new(),
                    span,
                });
                self.switch_to(some_block);
                let value = self.take_some(option, ty, span);
                self.assign_temp(ty, Rvalue::Use(value), span)
            }
            Some(Method::UnwrapOr) => {
                let option = self.consumed_temp(receiver);
                let default = match self.types.call_args.get(expr).map(Vec::as_slice) {
                    Some([ArgValue::Expr(default)]) => self.lower_consume(*default),
                    _ => return Operand::Const(Value::Nothing),
                };
                let default_temp = self.temp(ty);
                self.assign(Place::Local(default_temp), Rvalue::Use(default), span);
                let result = self.temp(ty);
                let discriminant = self.discriminant(Place::Local(option), span);
                let is_some =
                    self.binary_temp(BinaryOp::Eq, discriminant, variant_const(1), Ty::Bool, span);
                let (some_block, none_block, join) =
                    (self.new_block(), self.new_block(), self.new_block());
                self.branch(is_some, some_block, none_block);
                self.switch_to(some_block);
                let value = self.take_some(option, ty, span);
                self.assign(Place::Local(result), Rvalue::Use(value), span);
                self.goto(join);
                self.switch_to(none_block);
                let default = self.read_owned(Place::Local(default_temp), ty, span);
                self.assign(Place::Local(result), Rvalue::Use(default), span);
                self.goto(join);
                self.switch_to(join);
                Operand::Copy {
                    place: Place::Local(result),
                    span,
                }
            }
            Some(Method::Take) => {
                let place = self.changed_receiver(expr, receiver);
                let value = self.read_owned(place.clone(), ty, span);
                let result = self.assign_temp(ty, Rvalue::Use(value), span);
                self.assign(
                    place,
                    Rvalue::Variant {
                        ty,
                        variant: 0,
                        fields: Vec::new(),
                    },
                    span,
                );
                result
            }
            Some(Method::Unbox) => {
                let value = self.lower_consume(receiver);
                self.assign_temp(ty, Rvalue::Unbox(value), span)
            }
            Some(&method) => self.lower_collection_method(expr, method, receiver, ty, span),
            None => Operand::Const(Value::Nothing),
        }
    }

    /// The written arguments of a call, as (parameter position, expression), in the order
    /// they are written.
    fn source_order(&self, expr: hir::ExprId, values: &[ArgValue]) -> Vec<(usize, hir::ExprId)> {
        let written: Vec<hir::ExprId> = match &self.hir.exprs[expr] {
            hir::Expr::Call { args, .. }
            | hir::Expr::MethodCall { args, .. }
            | hir::Expr::Variant { args, .. } => args.iter().map(|arg| arg.value).collect(),
            _ => Vec::new(),
        };
        let mut order: Vec<(usize, hir::ExprId)> = values
            .iter()
            .enumerate()
            .filter_map(|(position, value)| match value {
                ArgValue::Expr(arg) => Some((position, *arg)),
                ArgValue::Default => None,
            })
            .collect();
        order.sort_by_key(|&(_, arg)| written.iter().position(|&w| w == arg));
        order
    }

    /// The argument `index` of a built-in method call.
    fn method_arg(&self, expr: hir::ExprId, index: usize) -> Option<hir::ExprId> {
        match self.types.call_args.get(expr)?.get(index)? {
            ArgValue::Expr(arg) => Some(*arg),
            ArgValue::Default => None,
        }
    }

    /// A method of a list, map or set.
    fn lower_collection_method(
        &mut self,
        expr: hir::ExprId,
        method: Method,
        receiver: hir::ExprId,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let collection_ty = self.expr_ty(receiver);
        let arg = |b: &Self, index| b.method_arg(expr, index);
        // A borrowed receiver is copied first if an argument may change it.
        let args_change = (0..2)
            .filter_map(|index| arg(self, index))
            .any(|value| self.may_change(value));
        match method {
            Method::Len | Method::IsEmpty => {
                let place = self.borrow_place(receiver);
                let len = self.assign_temp(INT, Rvalue::Len(place), span);
                if method == Method::Len {
                    return len;
                }
                self.binary_temp(BinaryOp::Eq, len, int_const(0), Ty::Bool, span)
            }
            Method::Clear => {
                let place = self.changed_receiver(expr, receiver);
                self.push(Statement::Clear(place));
                Operand::Const(Value::Nothing)
            }
            Method::Push => {
                let list = self.changed_receiver(expr, receiver);
                let Some(value) = arg(self, 0) else {
                    return Operand::Const(Value::Nothing);
                };
                let value = self.lower_consume(value);
                self.push(Statement::ListPush { list, value });
                Operand::Const(Value::Nothing)
            }
            Method::Pop => {
                let list = self.changed_receiver(expr, receiver);
                self.assign_temp(ty, Rvalue::ListPop(list), span)
            }
            Method::Insert => self.lower_insert(expr, receiver, collection_ty, ty, span),
            Method::Swap => {
                let list = self.changed_receiver(expr, receiver);
                let (Some(first), Some(second)) = (arg(self, 0), arg(self, 1)) else {
                    return Operand::Const(Value::Nothing);
                };
                let mut indexes = Vec::new();
                for index in [first, second] {
                    let local = self.temp(INT);
                    let value = self.lower_read(index);
                    self.assign(Place::Local(local), Rvalue::Use(value), span);
                    indexes.push(local);
                }
                for &index in &indexes {
                    self.check_index(&list, index, false, span);
                }
                let [first, second] = [indexes[0], indexes[1]].map(|local| Operand::Copy {
                    place: Place::Local(local),
                    span,
                });
                self.push(Statement::ListSwap {
                    list,
                    first,
                    second,
                });
                Operand::Const(Value::Nothing)
            }
            Method::Remove => self.lower_remove(expr, receiver, collection_ty, ty, span),
            Method::Get => {
                let place = self.with_eager(args_change, |b| b.borrow_place(receiver));
                let Some(first) = arg(self, 0) else {
                    return Operand::Const(Value::Nothing);
                };
                self.lower_get(place, collection_ty, first, ty, span)
            }
            Method::Contains => {
                let collection = self.with_eager(args_change, |b| b.borrow_place(receiver));
                let Some(first) = arg(self, 0) else {
                    return Operand::Const(Value::Nothing);
                };
                let value = self.lower_read(first);
                self.assign_temp(Ty::Bool, Rvalue::Contains { collection, value }, span)
            }
            Method::Keys | Method::Values => {
                let place = self.borrow_place(receiver);
                let rvalue = if method == Method::Keys {
                    Rvalue::MapKeys(place)
                } else {
                    Rvalue::MapValues(place)
                };
                self.assign_temp(ty, rvalue, span)
            }
            _ => unreachable!("not a collection method: {method:?}"),
        }
    }

    /// `insert` of a list, map or set.
    fn lower_insert(
        &mut self,
        expr: hir::ExprId,
        receiver: hir::ExprId,
        collection_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let arg = |b: &Self, index| b.method_arg(expr, index);
        let place = self.changed_receiver(expr, receiver);
        let (Some(first), second) = (arg(self, 0), arg(self, 1)) else {
            return Operand::Const(Value::Nothing);
        };
        // With named arguments, the value may be written (and evaluated) first.
        let values = self.types.call_args.get(expr).cloned().unwrap_or_default();
        let value_first = matches!(self.source_order(expr, &values).first(), Some(&(1, _)));
        match (collection_ty, second) {
            (Ty::List(_), Some(value)) => {
                let index = self.temp(INT);
                let early = value_first.then(|| self.lower_consume(value));
                let index_value = self.lower_read(first);
                self.assign(Place::Local(index), Rvalue::Use(index_value), span);
                let value = match early {
                    Some(value) => value,
                    None => self.lower_consume(value),
                };
                self.check_index(&place, index, true, span);
                let index = Operand::Copy {
                    place: Place::Local(index),
                    span,
                };
                self.push(Statement::ListInsert {
                    list: place,
                    index,
                    value,
                });
                Operand::Const(Value::Nothing)
            }
            (Ty::Map(_), Some(value)) => {
                let (key, value) = if value_first {
                    let value = self.lower_consume(value);
                    (self.lower_consume(first), value)
                } else {
                    let key = self.lower_consume(first);
                    (key, self.lower_consume(value))
                };
                self.assign_temp(
                    ty,
                    Rvalue::MapInsert {
                        map: place,
                        key,
                        value,
                    },
                    span,
                )
            }
            _ => {
                // A set: the element was added if no equal one was there.
                let key = self.lower_consume(first);
                let old = self.assign_temp(
                    Ty::option(Ty::Nothing),
                    Rvalue::MapInsert {
                        map: place,
                        key,
                        value: Operand::Const(Value::Nothing),
                    },
                    span,
                );
                self.is_variant(&old, 0, span)
            }
        }
    }

    /// `remove` of a list, map or set.
    fn lower_remove(
        &mut self,
        expr: hir::ExprId,
        receiver: hir::ExprId,
        collection_ty: Ty,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let arg = |b: &Self, index| b.method_arg(expr, index);
        let place = self.changed_receiver(expr, receiver);
        let Some(first) = arg(self, 0) else {
            return Operand::Const(Value::Nothing);
        };
        match collection_ty {
            Ty::List(_) => {
                let index = self.temp(INT);
                let index_value = self.lower_read(first);
                self.assign(Place::Local(index), Rvalue::Use(index_value), span);
                self.check_index(&place, index, false, span);
                let index = Operand::Copy {
                    place: Place::Local(index),
                    span,
                };
                self.assign_temp(ty, Rvalue::ListRemove { list: place, index }, span)
            }
            Ty::Map(_) => {
                let key = self.lower_read(first);
                self.assign_temp(ty, Rvalue::MapRemove { map: place, key }, span)
            }
            _ => {
                let key = self.lower_read(first);
                let old = self.assign_temp(
                    Ty::option(Ty::Nothing),
                    Rvalue::MapRemove { map: place, key },
                    span,
                );
                self.is_variant(&old, 1, span)
            }
        }
    }

    /// Whether the option in `option` (an operand of a temporary) is the variant `variant`.
    fn is_variant(&mut self, option: &Operand, variant: u32, span: Span) -> Operand {
        let Some(place) = option.place().cloned() else {
            return Operand::Const(Value::Bool(false));
        };
        let discriminant = self.discriminant(place, span);
        self.binary_temp(
            BinaryOp::Eq,
            discriminant,
            variant_const(variant),
            Ty::Bool,
            span,
        )
    }

    /// `[$list->get $i]` or `[$map->get $k]`: a copy of the element or value, or `none`.
    fn lower_get(
        &mut self,
        collection: Place,
        collection_ty: Ty,
        index: hir::ExprId,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let position = self.temp(INT);
        let found = if let Ty::Map(_) = collection_ty {
            let key = self.lower_read(index);
            self.assign(
                Place::Local(position),
                Rvalue::MapFind {
                    map: collection.clone(),
                    key,
                },
                span,
            );
            let at = Operand::Copy {
                place: Place::Local(position),
                span,
            };
            self.binary_temp(BinaryOp::Ge, at, int_const(0), Ty::Bool, span)
        } else {
            let value = self.lower_read(index);
            self.assign(Place::Local(position), Rvalue::Use(value), span);
            let at = Operand::Copy {
                place: Place::Local(position),
                span,
            };
            let len = self.assign_temp(INT, Rvalue::Len(collection.clone()), span);
            let below = self.binary_temp(BinaryOp::Lt, at.clone(), len, Ty::Bool, span);
            let not_negative = self.binary_temp(BinaryOp::Ge, at, int_const(0), Ty::Bool, span);
            // `below and not_negative`, without evaluating anything twice.
            let result = self.temp(Ty::Bool);
            let (check, done) = (self.new_block(), self.new_block());
            self.assign(Place::Local(result), Rvalue::Use(below.clone()), span);
            self.branch(below, check, done);
            self.switch_to(check);
            self.assign(Place::Local(result), Rvalue::Use(not_negative), span);
            self.goto(done);
            self.switch_to(done);
            Operand::Copy {
                place: Place::Local(result),
                span,
            }
        };
        let element = match collection_ty {
            Ty::Map(_) => Place::MapValue(Box::new(collection), position),
            _ => Place::Index(Box::new(collection), position),
        };
        let element_ty = match ty {
            Ty::Option(inner) => *inner,
            other => other,
        };
        let result = self.temp(ty);
        let (some_block, none_block, join) = (self.new_block(), self.new_block(), self.new_block());
        self.branch(found, some_block, none_block);
        self.switch_to(some_block);
        let copy = self.assign_temp(element_ty, Rvalue::Clone(element), span);
        self.assign(
            Place::Local(result),
            Rvalue::Variant {
                ty,
                variant: 1,
                fields: vec![to_move(copy)],
            },
            span,
        );
        self.goto(join);
        self.switch_to(none_block);
        self.assign(
            Place::Local(result),
            Rvalue::Variant {
                ty,
                variant: 0,
                fields: Vec::new(),
            },
            span,
        );
        self.goto(join);
        self.switch_to(join);
        Operand::Copy {
            place: Place::Local(result),
            span,
        }
    }

    /// The index of the variant held by the enum or option in `place`.
    fn discriminant(&mut self, place: Place, span: Span) -> Operand {
        self.assign_temp(Ty::Int(IntTy::U32), Rvalue::Discriminant(place), span)
    }

    /// Evaluates `expr` into a new temporary that owns its value.
    fn consumed_temp(&mut self, expr: hir::ExprId) -> LocalId {
        let ty = self.expr_ty(expr);
        let span = self.hir.expr_span(expr);
        let value = self.lower_consume(expr);
        let temp = self.temp(ty);
        self.assign(Place::Local(temp), Rvalue::Use(value), span);
        temp
    }

    /// Takes the value of type `ty` out of the `some` held by the temporary `option`, which
    /// is consumed.
    fn take_some(&mut self, option: LocalId, ty: Ty, span: Span) -> Operand {
        let place = Place::Local(option).variant_field(1, 0);
        if self.is_copy(ty) {
            return Operand::Copy { place, span };
        }
        let value = self.temp(ty);
        self.assign(
            Place::Local(value),
            Rvalue::Use(Operand::Move { place, span }),
            span,
        );
        self.push(Statement::MarkMoved(option));
        to_move(Operand::Copy {
            place: Place::Local(value),
            span,
        })
    }

    /// The value of `place`, taking ownership of it unless it is `Copy`.
    fn read_owned(&self, place: Place, ty: Ty, span: Span) -> Operand {
        if self.is_copy(ty) {
            Operand::Copy { place, span }
        } else {
            Operand::Move { place, span }
        }
    }

    fn lower_call(
        &mut self,
        expr: hir::ExprId,
        callee: Callee,
        args: &[hir::CallArg],
        ty: Ty,
        span: Span,
    ) -> Operand {
        match callee {
            Callee::Fn(id) => self.lower_fn_call(expr, id, ty, span, None),
            Callee::Builtin(builtin) => self.lower_builtin(builtin, args, ty, span),
            Callee::Error => Operand::Const(Value::Nothing),
        }
    }

    /// A call of a user-defined function, or of a method with its `receiver`.
    fn lower_fn_call(
        &mut self,
        expr: hir::ExprId,
        id: hir::FnId,
        ty: Ty,
        span: Span,
        receiver: Option<hir::ExprId>,
    ) -> Operand {
        let module = self.module;
        let function = &module.functions[id];
        let type_args: Box<[Ty]> = self
            .types
            .instances
            .get(expr)
            .map(|args| args.iter().map(|&arg| self.ty(arg)).collect())
            .unwrap_or_default();
        let param_ty = |index: usize| function.params[index].ty.value.subst(&type_args);
        let arg_values = self.types.call_args.get(expr).cloned().unwrap_or_default();
        // The arguments of a method call bind to the parameters after `self`.
        let skip = usize::from(receiver.is_some());
        // Written arguments are evaluated left to right as written, after the receiver;
        // defaults, which are constants, follow.
        let order = self.source_order(expr, &arg_values);
        let evaluated: Vec<hir::ExprId> = receiver
            .into_iter()
            .chain(order.iter().map(|&(_, arg)| arg))
            .collect();
        let eager = self.later_changes(&evaluated);
        let mut eager = eager.into_iter();
        let mut args: Vec<Option<CallArg>> = vec![None; arg_values.len() + skip];
        if let (Some(receiver), Some(self_param)) = (receiver, function.params.first()) {
            let arg = self.with_eager(eager.next().unwrap_or(false), |b| {
                b.lower_arg(receiver, self_param.convention, param_ty(0))
            });
            args[0] = Some(arg);
        }
        for &(position, arg) in &order {
            let param = &function.params[position + skip];
            let lowered = self.with_eager(eager.next().unwrap_or(false), |b| {
                b.lower_arg(arg, param.convention, param_ty(position + skip))
            });
            args[position + skip] = Some(lowered);
        }
        for (index, value) in arg_values.into_iter().enumerate() {
            let index = index + skip;
            let param = &function.params[index];
            let param_ty = param_ty(index);
            let arg = match value {
                ArgValue::Default => {
                    let value = Operand::Const(self.consts.default_value(id, index));
                    if param.convention == Convention::Read && !self.is_copy(param_ty) {
                        // A borrowed parameter refers to a temporary holding the default.
                        let temp = self.temp(param_ty);
                        self.assign(Place::Local(temp), Rvalue::Use(value), span);
                        CallArg::Ref {
                            place: Place::Local(temp),
                            mutable: false,
                            span,
                        }
                    } else {
                        CallArg::Value(value)
                    }
                }
                ArgValue::Expr(_) => continue,
            };
            args[index] = Some(arg);
        }
        let args: Vec<CallArg> = args
            .into_iter()
            .map(|arg| arg.expect("every parameter has an argument"))
            .collect();
        self.check_exclusive_args(&args);
        self.emit_call(id, type_args, args, ty, span)
    }

    /// Calls function `id` with type arguments `type_args` and arguments `args`, which are
    /// already checked; returns the result, of type `ty`.
    fn emit_call(
        &mut self,
        id: hir::FnId,
        type_args: Box<[Ty]>,
        args: Vec<CallArg>,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let (id, type_args) = resolve_trait_call(self.module, id, type_args);
        if let Some(prelude) = builtin_operator(self.module, id, &type_args) {
            return self.lower_builtin_operator(prelude, args, ty, span);
        }
        let function = &self.module.functions[id];
        if function.kind == hir::FnKind::Runtime {
            let intrinsic =
                intrinsic_of(function).expect("runtime functions are checked to be intrinsics");
            let args = args.into_iter().map(CallArg::into_operand).collect();
            return self.assign_temp(ty, Rvalue::Intrinsic { intrinsic, args }, span);
        }
        let func = self
            .instances
            .as_mut()
            .expect("only function bodies call functions")
            .request(id, type_args, span);
        let raises = self.module.functions[id].raises;
        self.emit_call_to(CallTarget::Direct(func), raises, args, ty, span)
    }

    /// Calls `func` with the arguments `args`, which are already checked; returns the
    /// result, of type `ty`. If the function `raises`, its error is raised here.
    fn emit_call_to(
        &mut self,
        func: CallTarget,
        raises: bool,
        args: Vec<CallArg>,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let destination = self.temp(ty);
        let target = (ty != Ty::Never).then(|| self.new_block());
        // The error of a function that raises one leaves through its own block.
        let on_error = raises.then(|| {
            let error = self.alloc_local(self.error_ty(), LocalMode::Value, None);
            (error, self.new_block())
        });
        self.terminate(Terminator::Call {
            func,
            args,
            destination: Place::Local(destination),
            target,
            on_error: on_error.map(|(error, block)| ErrorTarget {
                place: Place::Local(error),
                block,
            }),
        });
        if let Some((error, block)) = on_error {
            self.switch_to(block);
            self.record_trace(error, span);
            self.raise_from(error, span);
        }
        if let Some(target) = target {
            self.switch_to(target);
        }
        Operand::Copy {
            place: Place::Local(destination),
            span,
        }
    }

    /// The built-in operation of a prelude trait's function called on a built-in type, as in
    /// `($a + $b)` where `$a` has a type parameter type, in an instance where it is `i64`.
    fn lower_builtin_operator(
        &mut self,
        prelude: hir::PreludeTrait,
        args: Vec<CallArg>,
        ty: Ty,
        span: Span,
    ) -> Operand {
        let mut operands = args.into_iter().map(CallArg::into_operand);
        let first = operands.next().expect("operators have operands");
        let op = match prelude {
            hir::PreludeTrait::Neg => {
                let rvalue = Rvalue::Unary {
                    op: UnaryOp::Neg,
                    operand: first,
                };
                return self.assign_temp(ty, rvalue, span);
            }
            hir::PreludeTrait::Add => BinaryOp::Add,
            hir::PreludeTrait::Sub => BinaryOp::Sub,
            hir::PreludeTrait::Mul => BinaryOp::Mul,
            hir::PreludeTrait::Div => BinaryOp::Div,
            hir::PreludeTrait::Rem => BinaryOp::Rem,
            hir::PreludeTrait::Concat => BinaryOp::Concat,
            hir::PreludeTrait::Drop => unreachable!("built-in types do not implement `Drop`"),
        };
        let second = operands.next().expect("binary operators have two operands");
        self.binary_temp(op, first, second, ty, span)
    }

    /// An operator on values whose type implements its prelude trait: a call of the trait's
    /// function, which reads its operands in order.
    fn lower_operator_call(
        &mut self,
        expr: hir::ExprId,
        function: hir::FnId,
        operands: &[hir::ExprId],
        ty: Ty,
        span: Span,
    ) -> Operand {
        let type_args: Box<[Ty]> = self.types.instances[expr]
            .iter()
            .map(|&arg| self.ty(arg))
            .collect();
        let params = &self.module.functions[function].params;
        let eager = self.later_changes(operands);
        let mut args = Vec::with_capacity(operands.len());
        for ((&operand, param), eager) in operands.iter().zip(params).zip(eager) {
            let param_ty = param.ty.value.subst(&type_args);
            let arg = self.with_eager(eager, |b| b.lower_arg(operand, param.convention, param_ty));
            args.push(arg);
        }
        self.check_exclusive_args(&args);
        self.emit_call(function, type_args, args, ty, span)
    }

    /// An argument passed by the parameter's convention (spec section 12.2).
    fn lower_arg(&mut self, arg: hir::ExprId, convention: Convention, ty: Ty) -> CallArg {
        let span = self.hir.expr_span(arg);
        match convention {
            Convention::Owned => CallArg::Value(self.lower_consume(arg)),
            Convention::Read if self.is_copy(ty) => CallArg::Value(self.lower_read(arg)),
            Convention::Read => {
                let place = self.borrow_place(arg);
                CallArg::Ref {
                    place,
                    mutable: false,
                    span,
                }
            }
            Convention::Mut => {
                let place = self.mutable_place(arg);
                CallArg::Ref {
                    place,
                    mutable: true,
                    span,
                }
            }
        }
    }

    /// The place passed to a `mut` parameter: a variable that may be modified.
    fn mutable_place(&mut self, arg: hir::ExprId) -> Place {
        self.place_to_change(arg, None)
    }

    /// The receiver of a method call `expr` that changes it.
    fn changed_receiver(&mut self, expr: hir::ExprId, receiver: hir::ExprId) -> Place {
        let method = match &self.hir.exprs[expr] {
            hir::Expr::MethodCall { method, .. } => method.value.clone(),
            _ => String::new(),
        };
        self.place_to_change(receiver, Some(&method))
    }

    /// A place that code changes: the argument of a `mut` parameter, or the receiver of a
    /// method that changes it (`method`). It must be a variable that may be modified.
    fn place_to_change(&mut self, arg: hir::ExprId, method: Option<&str>) -> Place {
        let span = self.hir.expr_span(arg);
        let Some(place) = self.place_of(arg) else {
            let message = match method {
                Some(method) => format!("`{method}` changes its value, which must be a variable"),
                None => "a `mut` parameter needs a variable to modify".to_owned(),
            };
            self.error(
                Diagnostic::error(codes::MUT_ARGUMENT_NOT_VARIABLE, message, span)
                    .with_help("store the value in a `:local` variable first"),
            );
            return self.borrow_place(arg);
        };
        if let PlaceRoot::Capture(_) = place.root() {
            let name = match self.hir.exprs[self.root_expr(arg)] {
                hir::Expr::Local(local) => self.hir.locals[local].name.value.clone(),
                _ => String::new(),
            };
            self.error(
                Diagnostic::error(
                    codes::MUT_ARGUMENT_NOT_MUTABLE,
                    format!("cannot modify `{name}`: it is captured by this function value"),
                    span,
                )
                .with_help(
                    "a function value can only read what it captured; compute a new value instead",
                ),
            );
            return place;
        }
        if let PlaceRoot::Local(local) = place.root() {
            let decl = &self.locals[local];
            let mutable = match decl.mode {
                LocalMode::Ref { mutable } => mutable,
                LocalMode::Value => match self.hir.exprs[self.root_expr(arg)] {
                    hir::Expr::Local(hir_local) => self.hir.locals[hir_local].kind.is_mutable(),
                    _ => true,
                },
            };
            if !mutable {
                let name = decl
                    .user
                    .as_ref()
                    .map_or("this", |u| u.name.as_str())
                    .to_owned();
                let declared = decl.user.as_ref().map(|u| u.span);
                let message = match method {
                    Some(method) => {
                        format!("cannot call `{method}` on `{name}`: it cannot be modified")
                    }
                    None => {
                        format!("cannot pass `{name}` to a `mut` parameter: it cannot be modified")
                    }
                };
                let mut diagnostic =
                    Diagnostic::error(codes::MUT_ARGUMENT_NOT_MUTABLE, message, span)
                        .with_help("declare it with `:local`, or as a `mut` parameter");
                if let Some(declared) = declared {
                    diagnostic = diagnostic.with_secondary(declared, "declared here");
                }
                self.error(diagnostic);
            }
        }
        self.check_not_borrowed(&place, span);
        place
    }

    /// Reports a variable passed to a `mut` parameter that another argument of the same call
    /// also uses (spec section 12.2, exclusivity).
    fn check_exclusive_args(&mut self, args: &[CallArg]) {
        fn place_of(arg: &CallArg) -> Option<&Place> {
            match arg {
                CallArg::Ref { place, .. } => Some(place),
                CallArg::Value(operand) => operand.place(),
            }
        }
        for (index, arg) in args.iter().enumerate() {
            let CallArg::Ref {
                place,
                mutable: true,
                span,
            } = arg
            else {
                continue;
            };
            let conflict = args.iter().enumerate().find(|&(other, other_arg)| {
                other != index && place_of(other_arg).is_some_and(|other| other.overlaps(place))
            });
            if let Some((_, other)) = conflict {
                let other_span = match other {
                    CallArg::Ref { span, .. } => Some(*span),
                    CallArg::Value(Operand::Copy { span, .. } | Operand::Move { span, .. }) => {
                        Some(*span)
                    }
                    CallArg::Value(Operand::Const(_)) => None,
                };
                let mut diagnostic = Diagnostic::error(
                    codes::CONFLICTING_ARGUMENTS,
                    "a variable passed to a `mut` parameter cannot be used by another argument of the same call",
                    *span,
                );
                if let Some(other_span) = other_span {
                    diagnostic = diagnostic.with_secondary(other_span, "also used here");
                }
                self.error(diagnostic);
            }
        }
    }

    /// The default value of type `ty` (spec section 10.4).
    fn lower_default(&mut self, ty: Ty, span: Span) -> Operand {
        let constant = Operand::Const;
        match ty {
            Ty::Int(int) => constant(Value::Int { value: 0, ty: int }),
            Ty::Float(float) => constant(Value::Float {
                value: 0.0,
                ty: float,
            }),
            Ty::Bool => constant(Value::Bool(false)),
            Ty::Char => constant(Value::Char('\0')),
            Ty::Duration => constant(Value::Duration(0)),
            Ty::String => constant(Value::Str("".into())),
            Ty::List(_) => self.assign_temp(
                ty,
                Rvalue::List {
                    ty,
                    elements: Vec::new(),
                },
                span,
            ),
            Ty::Map(_) | Ty::Set(_) => self.assign_temp(
                ty,
                Rvalue::Map {
                    ty,
                    entries: Vec::new(),
                },
                span,
            ),
            Ty::Option(_) => self.assign_temp(
                ty,
                Rvalue::Variant {
                    ty,
                    variant: 0,
                    fields: Vec::new(),
                },
                span,
            ),
            Ty::Box(inner) => {
                let value = self.owned_default(*inner, span);
                self.assign_temp(ty, Rvalue::BoxNew(value), span)
            }
            Ty::Adt(adt) => {
                let id = hir::TypeId::from_raw(la_arena::RawIdx::from_u32(adt.index));
                let def = &self.module.types[id];
                if let Some(function) = def.function("default") {
                    return self.emit_call(function, adt.args.clone(), Vec::new(), ty, span);
                }
                let fields = (0..def.fields.len())
                    .map(|index| {
                        if def.fields[index].default.is_some() {
                            return Operand::Const(self.consts.field_default(id, index));
                        }
                        let index_u32 = u32::try_from(index).expect("few fields");
                        let field_ty = self.structs.field_ty(ty, index_u32);
                        self.owned_default(field_ty, span)
                    })
                    .collect();
                self.assign_temp(ty, Rvalue::Struct { ty, fields }, span)
            }
            _ => Operand::Const(Value::Nothing),
        }
    }

    /// The default value of type `ty`, owned by the operand's user.
    fn owned_default(&mut self, ty: Ty, span: Span) -> Operand {
        let value = self.lower_default(ty, span);
        if self.is_copy(ty) {
            value
        } else {
            to_move(value)
        }
    }

    fn lower_builtin(
        &mut self,
        builtin: Builtin,
        args: &[hir::CallArg],
        ty: Ty,
        span: Span,
    ) -> Operand {
        let first = args.first().map(|a| a.value);
        match builtin {
            Builtin::Default => self.lower_default(ty, span),
            Builtin::Put => {
                let parts = first.map(|arg| self.print_parts(arg)).unwrap_or_default();
                self.push(Statement::Print {
                    parts,
                    stream: Stream::Stdout,
                    newline: true,
                });
                Operand::Const(Value::Nothing)
            }
            Builtin::Assert => {
                let Some(cond) = first else {
                    return Operand::Const(Value::Nothing);
                };
                let (cond, values) = self.lower_assert_condition(cond, span);
                let fail = self.new_block();
                let ok = self.new_block();
                self.branch(cond, ok, fail);
                self.switch_to(fail);
                let mut message = args
                    .get(1)
                    .map(|a| self.print_parts(a.value))
                    .unwrap_or_default();
                message.extend(values);
                self.terminate(Terminator::Panic {
                    kind: PanicKind::Assertion,
                    message,
                    span,
                });
                self.switch_to(ok);
                Operand::Const(Value::Nothing)
            }
            Builtin::Panic => {
                let message = first.map(|arg| self.print_parts(arg)).unwrap_or_default();
                self.terminate(Terminator::Panic {
                    kind: PanicKind::Explicit,
                    message,
                    span,
                });
                Operand::Const(Value::Nothing)
            }
            Builtin::Nothing => Operand::Const(Value::Nothing),
            Builtin::ToStr | Builtin::TypeOf => {
                let parts = match (builtin, first) {
                    (Builtin::ToStr, Some(arg)) => self.print_parts(arg),
                    (_, Some(arg)) => vec![PrintPart::Text(self.expr_ty(arg).to_string())],
                    (_, None) => Vec::new(),
                };
                if let [PrintPart::Text(text)] = parts.as_slice() {
                    return Operand::Const(Value::Str(text.as_str().into()));
                }
                self.assign_temp(ty, Rvalue::Interpolate(parts), span)
            }
            Builtin::Len => {
                let Some(arg) = first else {
                    return Operand::Const(Value::Int {
                        value: 0,
                        ty: IntTy::I64,
                    });
                };
                if self.expr_ty(arg) == Ty::String {
                    let operand = self.lower_read(arg);
                    return self.assign_temp(ty, Rvalue::StringLen(operand), span);
                }
                let collection = self.borrow_place(arg);
                self.assign_temp(ty, Rvalue::Len(collection), span)
            }
        }
    }

    /// The condition of an `:assert`, and what its failure message adds: for a comparison of
    /// values that can be displayed, the value of each side, which are evaluated once.
    fn lower_assert_condition(
        &mut self,
        cond: hir::ExprId,
        span: Span,
    ) -> (Operand, Vec<PrintPart>) {
        let comparison = match self.hir.exprs[cond] {
            hir::Expr::Binary { op, lhs, rhs, .. } if self.types.methods.get(cond).is_none() => {
                let op = match op {
                    hir::BinaryOp::Eq => Some(BinaryOp::Eq),
                    hir::BinaryOp::Ne => Some(BinaryOp::Ne),
                    hir::BinaryOp::Lt => Some(BinaryOp::Lt),
                    hir::BinaryOp::Le => Some(BinaryOp::Le),
                    hir::BinaryOp::Gt => Some(BinaryOp::Gt),
                    hir::BinaryOp::Ge => Some(BinaryOp::Ge),
                    _ => None,
                };
                op.map(|op| (op, lhs, rhs))
            }
            _ => None,
        };
        let Some((op, lhs, rhs)) = comparison.filter(|&(_, lhs, rhs)| {
            self.structs.can_display(self.own_ty(lhs)) && self.structs.can_display(self.own_ty(rhs))
        }) else {
            return (self.lower_read(cond), Vec::new());
        };
        let left = self.with_eager(self.may_change(rhs), |b| b.lower_read(lhs));
        let right = self.lower_read(rhs);
        let result = self.binary_temp(op, left.clone(), right.clone(), Ty::Bool, span);
        let values = vec![
            PrintPart::Text("\n  left: ".to_owned()),
            PrintPart::Value(left),
            PrintPart::Text("\n right: ".to_owned()),
            PrintPart::Value(right),
        ];
        (result, values)
    }

    /// The pieces printed for an expression. Strings built from literals, constants,
    /// interpolation, `:tostr` and `:typeof` are printed piece by piece, without building
    /// the string.
    fn print_parts(&mut self, expr: hir::ExprId) -> Vec<PrintPart> {
        if self.wrapped(expr).is_some() || self.own_ty(expr) != Ty::String {
            return vec![PrintPart::Value(self.lower_read(expr))];
        }
        self.string_parts(expr)
    }

    /// The pieces of a string-valued expression (its own value, before any wrapping).
    fn string_parts(&mut self, expr: hir::ExprId) -> Vec<PrintPart> {
        match &self.hir.exprs[expr] {
            hir::Expr::String(parts) => {
                let values: Vec<hir::ExprId> = parts
                    .iter()
                    .filter_map(|part| match part {
                        StringPart::Expr(expr) => Some(*expr),
                        StringPart::Text(_) => None,
                    })
                    .collect();
                let mut eager = self.later_changes(&values).into_iter();
                let mut out = Vec::new();
                for part in parts {
                    match part {
                        StringPart::Text(text) if text.is_empty() => {}
                        StringPart::Text(text) => out.push(PrintPart::Text(text.clone())),
                        StringPart::Expr(part) => {
                            let eager = eager.next().unwrap_or(false);
                            let pieces = self.with_eager(eager, |b| b.print_parts(*part));
                            out.extend(pieces);
                        }
                    }
                }
                out
            }
            hir::Expr::Const(id) => vec![PrintPart::Text(self.consts.const_value(*id).display())],
            hir::Expr::Call {
                callee: Callee::Builtin(Builtin::TypeOf),
                args,
                ..
            } => {
                let name = args
                    .first()
                    .map_or_else(String::new, |a| self.expr_ty(a.value).to_string());
                vec![PrintPart::Text(name)]
            }
            hir::Expr::Call {
                callee: Callee::Builtin(Builtin::ToStr),
                args,
                ..
            } => args
                .first()
                .map(|a| self.print_parts(a.value))
                .unwrap_or_default(),
            _ => vec![PrintPart::Value(self.lower_read(expr))],
        }
    }
}

/// Turns a read of a temporary into a move of it.
/// An `int` constant.
fn int_const(value: i128) -> Operand {
    Operand::Const(Value::Int {
        value,
        ty: IntTy::I64,
    })
}

/// A variant index as an operand, for comparing discriminants.
fn variant_const(variant: u32) -> Operand {
    Operand::Const(Value::Int {
        value: i128::from(variant),
        ty: IntTy::U32,
    })
}

fn to_move(operand: Operand) -> Operand {
    match operand {
        Operand::Copy { place, span } => Operand::Move { place, span },
        other => other,
    }
}

/// The value of a literal, given the type inferred for it.
fn literal_value(literal: &hir::Literal, ty: Ty) -> Value {
    match (*literal, ty) {
        (hir::Literal::Int { value, negative }, Ty::Int(int)) => {
            let magnitude = i128::from(value);
            Value::int(if negative { -magnitude } else { magnitude }, int)
                .expect("the type checker checks literal ranges")
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "rounding to f32 is intended"
        )]
        (hir::Literal::Float(value), Ty::Float(float)) => Value::Float {
            value: match float {
                pika_hir::FloatTy::F64 => value,
                pika_hir::FloatTy::F32 => f64::from(value as f32),
            },
            ty: float,
        },
        (hir::Literal::Bool(value), _) => Value::Bool(value),
        (hir::Literal::Char(value), _) => Value::Char(value),
        (hir::Literal::Duration(nanos), _) => Value::Duration(nanos),
        _ => Value::Nothing,
    }
}
