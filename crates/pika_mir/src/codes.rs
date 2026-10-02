//! Stable codes for MIR diagnostics.

/// A variable read before it is assigned on every path.
pub const USE_BEFORE_ASSIGNMENT: &str = "E0401";
/// A constant whose evaluation at compile time panics.
pub const CONST_EVALUATION_FAILED: &str = "E0402";
/// A construct that type checks but that the compiler cannot run yet.
pub const NOT_RUNNABLE_YET: &str = "E0403";
/// A value used after it was moved.
pub const USE_AFTER_MOVE: &str = "E0404";
/// A variable passed to a `mut` parameter and to another argument of the same call.
pub const CONFLICTING_ARGUMENTS: &str = "E0405";
/// A `mut` argument that cannot be modified.
pub const MUT_ARGUMENT_NOT_MUTABLE: &str = "E0406";
/// A `mut` argument that is not a variable.
pub const MUT_ARGUMENT_NOT_VARIABLE: &str = "E0407";
/// A move out of a borrowed parameter or a global.
pub const MOVE_OUT_OF_BORROW: &str = "E0408";
/// A change to a value while a `:match` arm borrows it.
pub const BORROWED_BY_MATCH: &str = "E0409";
/// A generic function that needs instances with ever deeper type arguments.
pub const INSTANCE_TOO_DEEP: &str = "E0410";
/// A part moved out of a value whose type implements `Drop`.
pub const MOVE_OUT_OF_DROP: &str = "E0411";
/// A constant whose evaluation destroys a value whose type implements `Drop`.
pub const DROP_IN_CONSTANT: &str = "E0412";
/// A function of the runtime that is not an intrinsic, or whose signature does not match it.
pub const INVALID_INTRINSIC: &str = "E0413";

/// A `:local` that is never modified and could be a `:const`.
pub const COULD_BE_CONST: &str = "W0001";
