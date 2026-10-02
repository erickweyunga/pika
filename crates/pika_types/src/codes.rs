//! Stable codes for type checking diagnostics.

/// A value whose type differs from the expected type.
pub const MISMATCHED_TYPES: &str = "E0301";
/// A variable whose type cannot be inferred.
pub const CANNOT_INFER: &str = "E0302";
/// An operation that the operand's type does not support.
pub const UNSUPPORTED_OPERATION: &str = "E0303";
/// A literal that does not fit its type.
pub const LITERAL_OUT_OF_RANGE: &str = "E0304";
/// Call arguments that do not match the function's parameters.
pub const ARGUMENT_MISMATCH: &str = "E0305";
/// A function that can finish without returning its declared value.
pub const MISSING_RETURN: &str = "E0306";
/// A conversion with `as` that is not allowed.
pub const INVALID_CAST: &str = "E0307";
/// `:return` without a value in a function that returns one.
pub const MISSING_RETURN_VALUE: &str = "E0308";
/// Constants whose values depend on each other.
pub const CYCLIC_CONSTANT: &str = "E0310";
/// A method call on a type without that method.
pub const UNKNOWN_METHOD: &str = "E0312";
/// A field access on a type without that field.
pub const UNKNOWN_FIELD: &str = "E0313";
/// A derived trait that a field's type does not implement.
pub const INVALID_DERIVE: &str = "E0314";
/// A type that contains itself by value.
pub const RECURSIVE_STRUCT: &str = "E0315";
/// A `:match` whose arms do not cover every value.
pub const NON_EXHAUSTIVE_MATCH: &str = "E0316";
/// A map key or set element type without `Hash` and `Eq`.
pub const INVALID_KEY: &str = "E0317";
/// A value that cannot be indexed or iterated.
pub const NOT_A_COLLECTION: &str = "E0318";
/// A type argument that does not meet a bound of its type parameter.
pub const BOUND_NOT_MET: &str = "E0319";
/// A trait or a type's implementation of a trait that is not valid.
pub const INVALID_IMPL: &str = "E0320";
/// An error raised where nothing catches it or passes it on.
pub const UNCAUGHT_ERROR: &str = "E0321";
/// A value called that is not a function, or a function that cannot be a value.
pub const NOT_A_FUNCTION_VALUE: &str = "E0322";
/// A `main` function with parameters or a return value.
pub const INVALID_MAIN: &str = "E0311";

/// Code after a statement that never finishes.
pub const UNREACHABLE_CODE: &str = "W0101";
/// A `:match` arm that no value can reach.
pub const UNREACHABLE_PATTERN: &str = "W0102";
