//! Stable codes for syntax diagnostics.
//!
//! Codes are never reused or renumbered once released, so they can be searched for and
//! referenced from documentation.

/// A character that cannot start any token.
pub const UNEXPECTED_CHARACTER: &str = "E0001";
/// A string literal without a closing `"`.
pub const UNTERMINATED_STRING: &str = "E0002";
/// A character literal without a closing `'`.
pub const UNTERMINATED_CHAR: &str = "E0003";
/// An unknown or malformed escape sequence.
pub const INVALID_ESCAPE: &str = "E0004";
/// An integer literal with missing or invalid digits.
pub const INVALID_INT: &str = "E0005";
/// An integer literal larger than the largest supported integer.
pub const INT_TOO_LARGE: &str = "E0006";
/// A float literal that is malformed or out of range.
pub const INVALID_FLOAT: &str = "E0007";
/// A number literal followed by letters that are not a duration unit.
pub const INVALID_SUFFIX: &str = "E0008";
/// A malformed duration literal.
pub const INVALID_DURATION: &str = "E0009";
/// A backslash outside a string that is not at the end of a line.
pub const STRAY_BACKSLASH: &str = "E0010";
/// `==`, which is not an operator in Pika.
pub const DOUBLE_EQUALS: &str = "E0011";
/// An identifier containing non-ASCII characters.
pub const NON_ASCII_IDENT: &str = "E0012";
/// A `$` that is not followed by a variable name.
pub const STRAY_DOLLAR: &str = "E0013";
/// A raw string literal without its closing delimiter.
pub const UNTERMINATED_RAW_STRING: &str = "E0014";
/// A source file larger than 4 GiB.
pub const FILE_TOO_LARGE: &str = "E0015";
/// A character literal that does not contain exactly one character.
pub const INVALID_CHAR: &str = "E0016";

/// A token that does not fit the grammar at this position.
pub const EXPECTED: &str = "E0101";
/// An operator used in a command argument without parentheses.
pub const OPERATOR_IN_ARGUMENTS: &str = "E0102";
/// A named argument written with spaces around `=`.
pub const NAMED_ARG_SPACING: &str = "E0103";
/// A type annotation written with spaces around `:`.
pub const TYPE_ANNOTATION_SPACING: &str = "E0104";
/// A `:` that is not directly followed by a command name.
pub const COMMAND_NAME_SPACING: &str = "E0105";
/// Extra tokens after the end of a statement.
pub const EXPECTED_STATEMENT_END: &str = "E0106";
/// A tuple expression; tuples are reserved for a later version.
pub const TUPLES_RESERVED: &str = "E0107";
/// Chained comparison operators such as `$a < $b < $c`.
pub const CHAINED_COMPARISON: &str = "E0108";
/// A named argument that the command does not accept.
pub const UNKNOWN_ARGUMENT: &str = "E0109";
/// A required argument of a built-in command is missing.
pub const MISSING_ARGUMENT: &str = "E0110";
/// A type name used where a value is expected.
pub const TYPE_AS_VALUE: &str = "E0111";
/// A statement that does not start with a command.
pub const NOT_A_COMMAND: &str = "E0112";
/// A `$` on a name that is being declared or assigned.
pub const DOLLAR_IN_DECLARATION: &str = "E0113";
/// A path written with spaces around `/`.
pub const PATH_SPACING: &str = "E0114";
/// A closing delimiter without a matching opening delimiter.
pub const UNMATCHED_CLOSING: &str = "E0115";
/// An opening delimiter without its closing delimiter.
pub const UNCLOSED_DELIMITER: &str = "E0116";
