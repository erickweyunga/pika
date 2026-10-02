//! The mid-level intermediate representation (MIR) of Pika programs.
//!
//! A MIR body is a control-flow graph of basic blocks. Each block holds simple statements
//! (assignments of operations on operands, and output) and ends in one terminator (a jump, a
//! branch, a call, a return or a panic). Expressions are flattened into temporaries, and
//! control flow that is implicit in the source (`and`, `or`, loops, `:break`) is explicit.
//!
//! This crate builds MIR from the HIR and the inferred types ([`build_program`]), checks
//! ownership (values are assigned before use and not used after being moved) and inserts the
//! destruction of owned values ([`analyze`]), evaluates constants at compile time, and
//! interprets whole programs ([`interpret`]). Code generation consumes the same MIR.
//!
//! # Ownership in MIR
//!
//! A local slot either holds a value or, for `read` parameters of non-`Copy` types and all
//! `mut` parameters, a reference to a place owned by the caller. Reading a value is a
//! [`Operand::Copy`]; for a non-`Copy` type that is a borrowed read, which consumers such as
//! printing and comparisons use without taking ownership. Taking ownership is a
//! [`Operand::Move`], after which the place is uninitialized. [`Statement::Drop`] destroys a
//! value; the builder places drops at the end of each scope, and [`analyze`] removes those of
//! values that are certainly moved and guards those of values that may be moved with a
//! runtime flag.

mod build;
pub mod codes;
mod consts;
mod interp;
mod ownership;
mod types;
pub mod value;

pub use build::{build_program, kind_ty};
pub use interp::{InterpretError, interpret};
use std::collections::HashMap;

use la_arena::{Arena, ArenaMap, Idx};
pub use ownership::analyze;
use pika_diagnostics::Span;
use pika_hir::{FloatTy, GlobalId, IntTy};
pub use pika_runtime::PanicKind;
pub use pika_runtime::intrinsics::Intrinsic;
use pika_types::Ty;
pub use types::{AdtInfo, AdtShape, Types};
pub use value::Value;

/// Identifies a basic block in a [`Body`].
pub type BlockId = Idx<BasicBlock>;
/// Identifies a local slot in a [`Body`].
pub type LocalId = Idx<LocalDecl>;
/// Identifies a function instance: a function with concrete type arguments.
pub type InstanceId = Idx<Body>;

/// A whole program in MIR form.
#[derive(Clone, Debug, Default)]
pub struct Program {
    /// User-defined types.
    pub types: Types,
    /// The body of every function instance: each function without type parameters, and each
    /// generic function once for every list of type arguments it is called with.
    pub functions: Arena<Body>,
    /// The type and initial value of every module-level variable.
    pub globals: ArenaMap<GlobalId, GlobalInit>,
    /// The instance of the `drop` function of each struct or enum type with one that the
    /// program may destroy a value of.
    pub drop_fns: HashMap<Ty, InstanceId>,
    /// The instance of the `fmt` function of each struct or enum type with one that the
    /// program may display a value of.
    pub display_fns: HashMap<Ty, InstanceId>,
    /// The function to run.
    pub entry: Option<InstanceId>,
    /// Constructs that the code generator does not support yet, with the milestone that adds
    /// them. Programs using them can be checked but not run.
    pub unsupported: Vec<Unsupported>,
}

/// The initial state of a module-level variable.
#[derive(Clone, Debug)]
pub struct GlobalInit {
    /// The variable's name.
    pub name: String,
    /// The variable's type.
    pub ty: Ty,
    /// The value computed at compile time.
    pub value: Value,
}

/// A construct that cannot be compiled yet.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Unsupported {
    /// Where it is used.
    pub span: Span,
    /// What it is.
    pub feature: &'static str,
    /// The milestone that implements it.
    pub milestone: &'static str,
}

/// The control-flow graph of one function.
#[derive(Clone, Debug)]
pub struct Body {
    /// The function's name, for diagnostics and symbol names.
    pub name: String,
    /// Where the function is declared, the location reported for a stack overflow.
    pub span: Span,
    /// Local slots: the return value, parameters, user variables and temporaries.
    pub locals: Arena<LocalDecl>,
    /// The slot that holds the return value.
    pub return_local: LocalId,
    /// The slots that hold the parameters, in order.
    pub params: Vec<LocalId>,
    /// Basic blocks.
    pub blocks: Arena<BasicBlock>,
    /// The block where execution starts.
    pub entry: BlockId,
    /// Whether the function may leave with an error ([`Terminator::Raise`]) instead of
    /// returning.
    pub raises: bool,
    /// For the body of a function value: the types of the values it captured, which
    /// [`Place::Capture`] reads. Such a body receives them as a hidden first argument.
    pub env: Option<Vec<Ty>>,
}

/// A local slot.
#[derive(Clone, Debug)]
pub struct LocalDecl {
    /// The type of the value (for a reference, the type of the value referred to).
    pub ty: Ty,
    /// Whether the slot holds the value or refers to it.
    pub mode: LocalMode,
    /// For a variable written in the source: its name and declaration.
    pub user: Option<UserVariable>,
}

/// How a local slot holds its value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LocalMode {
    /// The slot owns its value.
    Value,
    /// The slot refers to a place owned elsewhere: a `read` parameter of a non-`Copy` type, or
    /// a `mut` parameter. Reads and writes of the slot go to that place.
    Ref {
        /// Whether the place may be modified (`mut`).
        mutable: bool,
    },
}

/// A variable written in the source.
#[derive(Clone, Debug)]
pub struct UserVariable {
    /// The variable's name.
    pub name: String,
    /// Where it is declared.
    pub span: Span,
    /// True for a `:local` declared without a value, which must be assigned before use.
    pub declared_without_value: bool,
}

/// A basic block: statements executed in order, then a terminator.
#[derive(Clone, Debug)]
pub struct BasicBlock {
    /// The statements.
    pub statements: Vec<Statement>,
    /// What happens after the statements.
    pub terminator: Terminator,
}

/// A statement inside a basic block.
#[derive(Clone, Debug)]
pub enum Statement {
    /// `place = value`
    Assign {
        /// The destination.
        place: Place,
        /// The computed value.
        value: Rvalue,
        /// The source of the assignment.
        span: Span,
    },
    /// Destroys the value in `place`, freeing what it owns. With a flag, only if the flag
    /// (a `bool` local) is true; the place is uninitialized afterwards.
    Drop {
        /// The place destroyed.
        place: Place,
        /// The local that says whether the place holds a value.
        flag: Option<LocalId>,
    },
    /// Makes the reference slot `local` refer to `place`: a binding of a `:match` arm that
    /// borrows part of the value matched.
    BindRef {
        /// A local whose mode is [`LocalMode::Ref`].
        local: LocalId,
        /// The place borrowed.
        place: Place,
    },
    /// Marks a local as no longer holding a value, without destroying it: everything it
    /// owned has been moved out or destroyed piece by piece.
    MarkMoved(LocalId),
    /// Appends a value to the list in `list`.
    ListPush {
        /// The list.
        list: Place,
        /// The value, moved in.
        value: Operand,
    },
    /// Inserts a value into the list in `list` at an index from 0 to its length, shifting
    /// later elements.
    ListInsert {
        /// The list.
        list: Place,
        /// The index (an `int` within bounds, checked before).
        index: Operand,
        /// The value, moved in.
        value: Operand,
    },
    /// Exchanges two elements of the list in `list`.
    ListSwap {
        /// The list.
        list: Place,
        /// The index of one element (an `int` within bounds, checked before).
        first: Operand,
        /// The index of the other.
        second: Operand,
    },
    /// Destroys every element of the list, map or set in `place`, which becomes empty.
    Clear(Place),
    /// Appends text and the text of values to the string or formatter at a place.
    Append {
        /// The string.
        target: Place,
        /// What to append, in order.
        parts: Vec<PrintPart>,
    },
    /// Writes text and values to an output stream.
    Print {
        /// What to print, in order.
        parts: Vec<PrintPart>,
        /// Where to print it.
        stream: Stream,
        /// Whether to end with a line break.
        newline: bool,
    },
}

/// What a call calls.
#[derive(Clone, Debug)]
pub enum CallTarget {
    /// A function instance.
    Direct(InstanceId),
    /// The function value in a place, which is borrowed for the call.
    Value(Place),
}

/// Where the error of a call goes: it is stored in `place`, and execution continues at
/// `block`.
#[derive(Clone, Debug)]
pub struct ErrorTarget {
    /// The place that receives the error, of type `Error`.
    pub place: Place,
    /// Where to continue.
    pub block: BlockId,
}

/// An output stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

/// A piece of printed output.
#[derive(Clone, Debug)]
pub enum PrintPart {
    /// Fixed text.
    Text(String),
    /// A value, formatted for display.
    Value(Operand),
}

/// A location that can be read and assigned.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Place {
    /// A local slot.
    Local(LocalId),
    /// A module-level variable.
    Global(GlobalId),
    /// A field of a struct held in another place, by index.
    Field(Box<Place>, u32),
    /// A field of a variant of an enum (or the value of `some`) held in another place, which
    /// must hold that variant.
    VariantField(Box<Place>, u32, u32),
    /// The value in a box held in another place.
    Deref(Box<Place>),
    /// The element of a list held in another place, at the index held by a local (an `int`
    /// within bounds, checked before).
    Index(Box<Place>, LocalId),
    /// The key of an entry of a map, or an element of a set, held in another place, at the
    /// position held by a local (an `int` within bounds, checked before).
    MapKey(Box<Place>, LocalId),
    /// The value of an entry of a map held in another place, at the position held by a local.
    MapValue(Box<Place>, LocalId),
    /// A value captured by the function value whose body is running, by position. It can
    /// be read and borrowed, not modified or moved.
    Capture(u32),
}

/// One step from a place to a part of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Projection {
    /// A field of a struct.
    Field(u32),
    /// A field of a variant: the variant's index and the field's index.
    VariantField(u32, u32),
    /// The value in a box.
    Deref,
    /// The list element at the index held by a local.
    Index(LocalId),
    /// The map key (or set element) at the position held by a local.
    MapKey(LocalId),
    /// The map value at the position held by a local.
    MapValue(LocalId),
}

impl Projection {
    /// Returns true if the steps may reach the same part: positions held by locals are not
    /// known before running.
    fn may_equal(self, other: Self) -> bool {
        match (self, other) {
            (Self::Index(_), Self::Index(_))
            | (Self::MapKey(_), Self::MapKey(_))
            | (Self::MapValue(_), Self::MapValue(_)) => true,
            (a, b) => a == b,
        }
    }
}

/// The variable a place is part of.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PlaceRoot {
    /// A local slot.
    Local(LocalId),
    /// A module-level variable.
    Global(GlobalId),
    /// A captured value.
    Capture(u32),
}

impl Place {
    /// A field of this place.
    #[must_use]
    pub fn field(&self, index: u32) -> Self {
        Self::Field(Box::new(self.clone()), index)
    }

    /// A field of a variant of the enum in this place.
    #[must_use]
    pub fn variant_field(&self, variant: u32, index: u32) -> Self {
        Self::VariantField(Box::new(self.clone()), variant, index)
    }

    /// The value in the box in this place.
    #[must_use]
    pub fn deref(&self) -> Self {
        Self::Deref(Box::new(self.clone()))
    }

    /// The place this one is a part of, if it is a part.
    pub fn base(&self) -> Option<&Self> {
        match self {
            Self::Local(_) | Self::Global(_) | Self::Capture(_) => None,
            Self::Field(base, _)
            | Self::VariantField(base, ..)
            | Self::Deref(base)
            | Self::Index(base, _)
            | Self::MapKey(base, _)
            | Self::MapValue(base, _) => Some(base),
        }
    }

    /// The local slot, if the place is a whole local.
    pub fn as_local(&self) -> Option<LocalId> {
        match self {
            Self::Local(local) => Some(*local),
            _ => None,
        }
    }

    /// The variable this place is part of, and the steps leading to it from there.
    pub fn path(&self) -> (PlaceRoot, Vec<Projection>) {
        let (base, step) = match self {
            Self::Local(local) => return (PlaceRoot::Local(*local), Vec::new()),
            Self::Global(global) => return (PlaceRoot::Global(*global), Vec::new()),
            Self::Capture(index) => return (PlaceRoot::Capture(*index), Vec::new()),
            Self::Field(base, index) => (base, Projection::Field(*index)),
            Self::VariantField(base, variant, index) => {
                (base, Projection::VariantField(*variant, *index))
            }
            Self::Deref(base) => (base, Projection::Deref),
            Self::Index(base, index) => (base, Projection::Index(*index)),
            Self::MapKey(base, index) => (base, Projection::MapKey(*index)),
            Self::MapValue(base, index) => (base, Projection::MapValue(*index)),
        };
        let (root, mut steps) = base.path();
        steps.push(step);
        (root, steps)
    }

    /// The variable this place is part of.
    pub fn root(&self) -> PlaceRoot {
        self.path().0
    }

    /// Returns true if the places may share memory: one is the other or a part of it.
    pub fn overlaps(&self, other: &Self) -> bool {
        let ((root, path), (other_root, other_path)) = (self.path(), other.path());
        root == other_root && path.iter().zip(&other_path).all(|(a, b)| a.may_equal(*b))
    }
}

/// An input of an operation.
#[derive(Clone, Debug)]
pub enum Operand {
    /// The current value of a place. For a non-`Copy` type, a borrowed read: the consumer
    /// reads the value without taking ownership.
    Copy {
        /// The place read.
        place: Place,
        /// Where it is read in the source.
        span: Span,
    },
    /// The value of a place, whose ownership is transferred: the place is uninitialized
    /// afterwards.
    Move {
        /// The place moved from.
        place: Place,
        /// Where it is moved in the source.
        span: Span,
    },
    /// A constant.
    Const(Value),
}

impl Operand {
    /// The place read or moved, if any.
    pub fn place(&self) -> Option<&Place> {
        match self {
            Self::Copy { place, .. } | Self::Move { place, .. } => Some(place),
            Self::Const(_) => None,
        }
    }
}

/// An argument of a call.
#[derive(Clone, Debug)]
pub enum CallArg {
    /// A value passed by copy or move.
    Value(Operand),
    /// A reference to a place, for a `read` parameter of a non-`Copy` type or a `mut`
    /// parameter.
    Ref {
        /// The place referred to.
        place: Place,
        /// Whether the callee may modify it.
        mutable: bool,
        /// Where the argument is written.
        span: Span,
    },
}

impl CallArg {
    /// The argument as an operand that reads it: a reference becomes a borrowed read.
    pub fn into_operand(self) -> Operand {
        match self {
            Self::Value(operand) => operand,
            Self::Ref { place, span, .. } => Operand::Copy { place, span },
        }
    }
}

/// A computation whose result is assigned to a place.
#[derive(Clone, Debug)]
pub enum Rvalue {
    /// The operand itself.
    Use(Operand),
    /// A unary operation.
    Unary {
        /// The operation.
        op: UnaryOp,
        /// Its input.
        operand: Operand,
    },
    /// A binary operation.
    Binary {
        /// The operation.
        op: BinaryOp,
        /// The left input.
        lhs: Operand,
        /// The right input.
        rhs: Operand,
    },
    /// A conversion.
    Cast {
        /// How to convert.
        kind: CastKind,
        /// The value converted.
        operand: Operand,
        /// The target type.
        to: Ty,
    },
    /// A new string built from pieces.
    Interpolate(Vec<PrintPart>),
    /// A deep copy of the value in a place.
    Clone(Place),
    /// A new struct value from the values of its fields, in order.
    Struct {
        /// The struct's type.
        ty: Ty,
        /// The fields' values; non-`Copy` values are moved.
        fields: Vec<Operand>,
    },
    /// A new value of an enum or option: a variant with the values of its fields. Options
    /// are `none` (variant 0) and `some` (variant 1, with one field).
    Variant {
        /// The enum or option type.
        ty: Ty,
        /// The variant's index.
        variant: u32,
        /// The fields' values; non-`Copy` values are moved.
        fields: Vec<Operand>,
    },
    /// The index of the variant held by the enum or option in a place, as a `u32`.
    Discriminant(Place),
    /// A new box holding a value, which is moved onto the heap.
    BoxNew(Operand),
    /// The value of a box, which is moved out; the box is freed.
    Unbox(Operand),
    /// A new list of elements, moved in.
    List {
        /// The list type.
        ty: Ty,
        /// The elements, in order.
        elements: Vec<Operand>,
    },
    /// A new map or set from entries, moved in and inserted in order: a later entry with the
    /// same key replaces the value of an earlier one, keeping its position. The values of a
    /// set's entries are `nothing`.
    Map {
        /// The map or set type.
        ty: Ty,
        /// The keys and values, in order.
        entries: Vec<(Operand, Operand)>,
    },
    /// The number of elements of the list, map or set in a place, as an `int`.
    Len(Place),
    /// The position of a key in the map or set in a place, or -1, as an `int`.
    MapFind {
        /// The map or set.
        map: Place,
        /// The key, read.
        key: Operand,
    },
    /// The last element of the list in a place, removed, as an option.
    ListPop(Place),
    /// The element of the list in a place at an index (within bounds), removed; later
    /// elements shift down.
    ListRemove {
        /// The list.
        list: Place,
        /// The index.
        index: Operand,
    },
    /// Inserts a key and value into the map or set in a place: a new entry at the end, or a
    /// new value for an existing key (whose new copy of the key is destroyed). The result is
    /// the old value, as an option.
    MapInsert {
        /// The map or set.
        map: Place,
        /// The key, moved in.
        key: Operand,
        /// The value, moved in.
        value: Operand,
    },
    /// Removes the entry with a key from the map or set in a place, destroying its key; the
    /// result is its value, as an option. Later entries keep their order.
    MapRemove {
        /// The map or set.
        map: Place,
        /// The key, read.
        key: Operand,
    },
    /// A new list of copies of the keys of the map in a place, in order.
    MapKeys(Place),
    /// A new list of copies of the values of the map in a place, in order.
    MapValues(Place),
    /// Whether the list or set in a place has an element equal to a value, or the map a key.
    Contains {
        /// The collection.
        collection: Place,
        /// The value, read.
        value: Operand,
    },
    /// The length in bytes of a string.
    StringLen(Operand),
    /// The result of an intrinsic of the runtime, called with arguments that it reads.
    Intrinsic {
        /// The intrinsic.
        intrinsic: Intrinsic,
        /// The arguments, one per parameter.
        args: Vec<Operand>,
    },
    /// A new function value: the body `func` (whose [`Body::env`] gives the captures' types)
    /// with the captured values, moved in.
    Closure {
        /// The function value's type.
        ty: Ty,
        /// The function value's body.
        func: InstanceId,
        /// The captured values, in order.
        captures: Vec<Operand>,
    },
}

/// A unary operation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnaryOp {
    /// Negation, which panics on overflow (`-i8::MIN`).
    Neg,
    /// Logical not.
    Not,
    /// Bitwise not.
    BitNot,
}

/// A binary operation. Arithmetic panics on overflow, division by zero and oversized shifts
/// unless it is one of the wrapping variants, which compiler-generated code uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(
    missing_docs,
    reason = "the operations are named after their operators"
)]
pub enum BinaryOp {
    Add,
    Sub,
    Mul,
    Div,
    Rem,
    WrappingAdd,
    WrappingSub,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    /// String concatenation, producing a new string.
    Concat,
    /// Substring test: whether the left string occurs in the right one (`in`).
    Contains,
}

impl BinaryOp {
    /// Returns true for comparisons, whose result is a `bool`.
    pub fn is_comparison(self) -> bool {
        matches!(
            self,
            Self::Eq | Self::Ne | Self::Lt | Self::Le | Self::Gt | Self::Ge | Self::Contains
        )
    }
}

/// How a cast converts.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CastKind {
    /// A conversion written with `as` (spec section 5.4): integer conversions that lose
    /// information panic, float-to-integer conversions saturate.
    As,
    /// Reinterprets the bits of an integer as another integer type of the same width.
    Reinterpret,
}

/// How a basic block ends.
#[derive(Clone, Debug)]
pub enum Terminator {
    /// Continues in another block.
    Goto(BlockId),
    /// Continues in `then_block` if `cond` is true, else in `else_block`.
    If {
        /// A `bool` operand.
        cond: Operand,
        /// Where to go if true.
        then_block: BlockId,
        /// Where to go if false.
        else_block: BlockId,
    },
    /// Calls a function and stores its result.
    Call {
        /// The function called.
        func: CallTarget,
        /// The arguments, in parameter order.
        args: Vec<CallArg>,
        /// Where the result goes.
        destination: Place,
        /// Where to continue; `None` if the function never returns.
        target: Option<BlockId>,
        /// For a function that may raise an error: where the error goes, and where to
        /// continue with it.
        on_error: Option<ErrorTarget>,
    },
    /// Leaves the function with an error, which goes to the caller's [`ErrorTarget`].
    Raise(Operand),
    /// Returns from the function with the value of the return slot.
    Return,
    /// Stops the program with an error.
    Panic {
        /// Why.
        kind: PanicKind,
        /// The message written by the program, for `:panic` and `:assert`.
        message: Vec<PrintPart>,
        /// Where in the source.
        span: Span,
    },
    /// Cannot be reached.
    Unreachable,
}

impl Terminator {
    /// The blocks that can follow this terminator.
    pub fn successors(&self) -> Vec<BlockId> {
        match self {
            Self::Goto(target) => vec![*target],
            Self::If {
                then_block,
                else_block,
                ..
            } => vec![*then_block, *else_block],
            Self::Call {
                target, on_error, ..
            } => target
                .iter()
                .copied()
                .chain(on_error.as_ref().map(|e| e.block))
                .collect(),
            Self::Return | Self::Raise(_) | Self::Panic { .. } | Self::Unreachable => Vec::new(),
        }
    }
}

impl Program {
    /// The type of an operand of `body`.
    pub fn operand_ty(&self, body: &Body, operand: &Operand) -> Ty {
        match operand {
            Operand::Copy { place, .. } | Operand::Move { place, .. } => self.place_ty(body, place),
            Operand::Const(value) => value.ty(),
        }
    }

    /// The type of a place of `body`.
    pub fn place_ty(&self, body: &Body, place: &Place) -> Ty {
        place_ty(&self.types, body, place, &|global| {
            self.globals.get(global).map_or(Ty::Error, |g| g.ty)
        })
    }
}

/// The type of a place of `body`, with the types of globals given by `global_ty`.
pub(crate) fn place_ty(
    types: &Types,
    body: &Body,
    place: &Place,
    global_ty: &dyn Fn(GlobalId) -> Ty,
) -> Ty {
    let base_ty = |base: &Place| place_ty(types, body, base, global_ty);
    match place {
        Place::Local(local) => body.locals[*local].ty,
        Place::Global(global) => global_ty(*global),
        Place::Capture(index) => body
            .env
            .as_ref()
            .and_then(|captures| captures.get(*index as usize).copied())
            .unwrap_or(Ty::Error),
        Place::Field(base, index) => types.field_ty(base_ty(base), *index),
        Place::VariantField(base, variant, index) => {
            types.variant_field_ty(base_ty(base), *variant, *index)
        }
        Place::Deref(base) => match base_ty(base) {
            Ty::Box(inner) => *inner,
            _ => Ty::Error,
        },
        Place::Index(base, _) | Place::MapKey(base, _) | Place::MapValue(base, _) => {
            part_ty(base_ty(base), place)
        }
    }
}

/// The type of an element, key or value place, given the type of its collection.
pub fn part_ty(collection: Ty, place: &Place) -> Ty {
    match (collection, place) {
        (Ty::List(element), Place::Index(..)) | (Ty::Set(element), Place::MapKey(..)) => *element,
        (Ty::Map(map), Place::MapKey(..)) => map.key,
        (Ty::Map(map), Place::MapValue(..)) => map.value,
        (Ty::Set(_), Place::MapValue(..)) => Ty::Nothing,
        _ => Ty::Error,
    }
}

/// The unsigned integer type with the same width as `int`.
pub fn unsigned_of(int: IntTy) -> IntTy {
    match int {
        IntTy::I8 | IntTy::U8 => IntTy::U8,
        IntTy::I16 | IntTy::U16 => IntTy::U16,
        IntTy::I32 | IntTy::U32 => IntTy::U32,
        IntTy::I64 | IntTy::U64 => IntTy::U64,
    }
}

/// The floating-point type of a value of type `ty`, if it is one.
pub fn float_of(ty: Ty) -> Option<FloatTy> {
    match ty {
        Ty::Float(float) => Some(float),
        _ => None,
    }
}
