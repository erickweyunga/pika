//! Type inference and checking for Pika programs.
//!
//! [`check_module`] infers the type of every expression and local variable in a
//! [`pika_hir::Module`] and reports type errors. Inference is bidirectional and local to each
//! function: signatures are always explicit (spec section 5.5), so functions can be checked
//! independently. Integer and float literals get type variables that default to `i64` and
//! `f64` when nothing else constrains them.

pub mod codes;
mod exhaustive;
mod infer;
mod ty;

pub use infer::implied_bounds;
use la_arena::ArenaMap;
use pika_diagnostics::Diagnostic;
use pika_hir::{ConstId, ExprId, FnId, GlobalId, LocalId, Module, PatId, TypeId};
pub use pika_ty::{Ty, TyVar};

/// The value passed to one parameter in a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ArgValue {
    /// An argument written at the call site.
    Expr(ExprId),
    /// The parameter's default value.
    Default,
}

/// What a method call invokes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Method {
    /// `[$value->clone]`: a deep copy.
    Clone,
    /// `$out->write value`: appends the text of a value to a `Formatter`.
    Write,
    /// `[$value->field args]` for a field holding a function value: a call of that value.
    CallField(u32),
    /// A method of a user-defined type: a function whose first parameter is `self`.
    Fn(FnId),
    /// `[$option->is_some]`
    IsSome,
    /// `[$option->is_none]`
    IsNone,
    /// `[$option->unwrap]`: the value, or a panic for `none`. Takes the option.
    Unwrap,
    /// `[$option->unwrap_or default]`: the value, or `default` for `none`. Takes both.
    UnwrapOr,
    /// `[$option->take]`: the value, leaving `none` in its place.
    Take,
    /// `[$box->unbox]`: the value, out of its box. Takes the box.
    Unbox,
    /// `[$list->push value]`
    Push,
    /// `[$list->pop]`: the last element, removed, or `none`.
    Pop,
    /// `[$list->swap i j]`: exchanges two elements.
    Swap,
    /// `[$list->insert index value]`, `[$map->insert key value]` (the old value, if any),
    /// `[$set->insert value]` (whether it was added).
    Insert,
    /// `[$list->remove index]` (the element), `[$map->remove key]` (the value, if any),
    /// `[$set->remove value]` (whether it was there).
    Remove,
    /// `[$list->get index]`, `[$map->get key]`: a copy of the element or value, if any.
    Get,
    /// `[$c->contains value]`: whether a list or set has the value, or a map the key.
    Contains,
    /// `[$c->len]`
    Len,
    /// `[$c->is_empty]`
    IsEmpty,
    /// `[$c->clear]`: removes every element.
    Clear,
    /// `[$map->keys]`: a list of copies of the keys, in order.
    Keys,
    /// `[$map->values]`: a list of copies of the values, in order.
    Values,
}

/// What a `->name` access on a value reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Projection {
    /// The field with this index of a struct.
    Field(usize),
    /// The value in a box: `$b->value`.
    BoxValue,
}

/// The types of the expressions and locals of one body.
#[derive(Clone, Debug, Default)]
pub struct BodyTypes {
    /// The type of every expression.
    pub exprs: ArenaMap<ExprId, Ty>,
    /// The type of every local variable and parameter.
    pub locals: ArenaMap<LocalId, Ty>,
    /// For every call to a user-defined function, the value of each parameter, in parameter
    /// order. Positional and named arguments are already matched to parameters.
    pub call_args: ArenaMap<ExprId, Vec<ArgValue>>,
    /// The method invoked by every method call.
    pub methods: ArenaMap<ExprId, Method>,
    /// What every `->name` access on a value reads.
    pub projections: ArenaMap<ExprId, Projection>,
    /// The type of every pattern of a `:match` arm.
    pub pats: ArenaMap<PatId, Ty>,
    /// The type arguments of every call of a generic function (those of its type, then its
    /// own).
    pub instances: ArenaMap<ExprId, Vec<Ty>>,
    /// Expressions whose value is implicitly wrapped in `some`, with the option type. The
    /// expression's own type is in `exprs`.
    pub wrapped: ArenaMap<ExprId, Ty>,
}

/// The result of type checking a module.
#[derive(Clone, Debug, Default)]
pub struct TypeckResult {
    /// Type errors and warnings, ordered by position.
    pub diagnostics: Vec<Diagnostic>,
    /// The types inside each function.
    pub functions: ArenaMap<FnId, BodyTypes>,
    /// The type of each module-level constant.
    pub consts: ArenaMap<ConstId, Ty>,
    /// The type of each module-level variable.
    pub globals: ArenaMap<GlobalId, Ty>,
    /// The types inside the initializer of each constant.
    pub const_bodies: ArenaMap<ConstId, BodyTypes>,
    /// The types inside the initializer of each global.
    pub global_bodies: ArenaMap<GlobalId, BodyTypes>,
    /// The types inside the field defaults of each type.
    pub type_bodies: ArenaMap<TypeId, BodyTypes>,
}

/// Infers and checks the types of a whole module.
pub fn check_module(module: &Module) -> TypeckResult {
    infer::ModuleChecker::new(module).run()
}
