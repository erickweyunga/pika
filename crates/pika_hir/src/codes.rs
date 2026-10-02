//! Stable codes for name resolution diagnostics.

/// A variable that is not declared in scope.
pub const UNRESOLVED_VARIABLE: &str = "E0201";
/// A command that is neither built in nor declared.
pub const UNKNOWN_COMMAND: &str = "E0202";
/// A type name that is not known.
pub const UNKNOWN_TYPE: &str = "E0203";
/// Two module-level declarations with the same name.
pub const DUPLICATE_ITEM: &str = "E0204";
/// Two variables or parameters with the same name in one scope.
pub const DUPLICATE_LOCAL: &str = "E0205";
/// A function named after a built-in command.
pub const RESERVED_NAME: &str = "E0206";
/// `:set` on something that cannot be reassigned.
pub const IMMUTABLE_ASSIGNMENT: &str = "E0207";
/// A language feature that a later milestone implements.
pub const NOT_SUPPORTED_YET: &str = "E0208";
/// `:break` or `:continue` outside a loop.
pub const OUTSIDE_LOOP: &str = "E0209";
/// A file with both `:fn main` and top-level statements.
pub const MAIN_AND_SCRIPT: &str = "E0210";
/// `:global` inside a function.
pub const GLOBAL_IN_FUNCTION: &str = "E0211";
/// A declaration that belongs at the top level of a file, written inside a function.
pub const NESTED_FUNCTION: &str = "E0212";
/// A `:set` target that is not a variable.
pub const INVALID_ASSIGNMENT_TARGET: &str = "E0213";
/// A constant or global initializer that cannot be evaluated at compile time.
pub const NOT_CONST_EVALUABLE: &str = "E0214";
/// A declaration missing a required part, such as `:const` without a value.
pub const INCOMPLETE_DECLARATION: &str = "E0215";
/// The same named argument given twice.
pub const DUPLICATE_ARGUMENT: &str = "E0216";
/// Arguments that a built-in form does not accept in this combination.
pub const INVALID_FORM_ARGUMENTS: &str = "E0217";
/// A `self` parameter outside a method, or not first.
pub const INVALID_SELF: &str = "E0218";
/// A struct literal or field access that does not match the struct.
pub const FIELD_MISMATCH: &str = "E0219";
/// A pattern that cannot be used in `:match`.
pub const INVALID_PATTERN: &str = "E0220";
/// A bound or `impl=` entry that is not a trait.
pub const INVALID_BOUND: &str = "E0221";
/// A path whose module or item does not exist.
pub const UNRESOLVED_PATH: &str = "E0222";
/// An item, field or method used outside the module it is private to (spec section 13.3).
pub const PRIVATE_ITEM: &str = "E0223";
/// A `:use` whose name is also the name of a package, or whose path names both a module and
/// an item.
pub const AMBIGUOUS_PATH: &str = "E0224";
/// Top-level statements in a module that is not the program's entry module.
pub const STATEMENTS_OUTSIDE_ENTRY: &str = "E0225";
/// A function of the runtime (`:extern lib="pika"`) declared outside the standard library, or
/// with parts it cannot have.
pub const INVALID_RUNTIME_FUNCTION: &str = "E0226";
/// An `:impl` outside the standard library, or not of a built-in type.
pub const INVALID_IMPL: &str = "E0227";
