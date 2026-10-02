//! Token definitions.

use pika_diagnostics::Span;

/// The kind of a token.
///
/// The lexer is lossless: every byte of the source belongs to exactly one token, including
/// whitespace and comments ("trivia"). Concatenating the text of all tokens reproduces the input.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TokenKind {
    // Trivia.
    /// Spaces and tabs (and a leading byte order mark).
    Whitespace,
    /// `# ...` up to, not including, the end of the line.
    Comment,
    /// `## ...` up to, not including, the end of the line.
    DocComment,
    /// A backslash followed by a line break, which joins two physical lines.
    LineContinuation,

    /// A line break: `\n`, `\r\n` or `\r`. Significant inside blocks, ignored elsewhere.
    Newline,

    // Names.
    /// An identifier such as `put` or `Point`.
    Ident,
    /// A variable read such as `$name`. The text includes the `$`.
    Variable,
    /// `_`
    Underscore,
    /// `and`
    KwAnd,
    /// `or`
    KwOr,
    /// `in`
    KwIn,
    /// `as`
    KwAs,
    /// `true`
    KwTrue,
    /// `false`
    KwFalse,
    /// `none`
    KwNone,
    /// `some`
    KwSome,
    /// `self`
    KwSelf,
    /// `Self`
    KwSelfType,

    // Literals.
    /// An integer literal: `42`, `0xFF`, `0o755`, `0b1010`, `1_000`.
    Int,
    /// A float literal: `3.14`, `1e9`, `2.5e-3`.
    Float,
    /// A duration literal: `500ms`, `1m30s`.
    Duration,
    /// A character literal: `'a'`, `'\n'`.
    Char,
    /// A raw string literal: `r"..."`, `r#"..."#`.
    RawString,
    /// The opening `"` of a string literal.
    StringStart,
    /// Literal text inside a string, with escape sequences still encoded.
    StringText,
    /// The closing `"` of a string literal.
    StringEnd,
    /// `$name` interpolated inside a string.
    InterpVar,
    /// `$(` starting an interpolated expression inside a string. Closed by [`TokenKind::RParen`].
    InterpParen,
    /// `$[` starting an interpolated command inside a string. Closed by [`TokenKind::RBracket`].
    InterpBracket,

    // Delimiters.
    /// `(`
    LParen,
    /// `)`
    RParen,
    /// `[`
    LBracket,
    /// `]`
    RBracket,
    /// `{`
    LBrace,
    /// `}`
    RBrace,
    /// `:`
    Colon,
    /// `;`
    Semi,
    /// `,`
    Comma,

    // Operators.
    /// `+`
    Plus,
    /// `-`
    Minus,
    /// `*`
    Star,
    /// `/`
    Slash,
    /// `%`
    Percent,
    /// `!`
    Bang,
    /// `~`
    Tilde,
    /// `&`
    Amp,
    /// `|`
    Pipe,
    /// `^`
    Caret,
    /// `.`
    Dot,
    /// `=`
    Eq,
    /// `!=`
    Ne,
    /// `<`
    Lt,
    /// `<=`
    Le,
    /// `<<`
    Shl,
    /// `>`. The lexer never produces `>=` or `>>`: the parser glues joined `>` tokens so that
    /// nested generics such as `List<List<i64>>` and defaults such as `xs:List<i64>={}` work.
    Gt,
    /// `&&`
    AndAnd,
    /// `||`
    OrOr,
    /// `->`
    Arrow,
    /// `?`, used for option types such as `i64?`.
    Question,

    /// A byte sequence that is not a valid token. A diagnostic is always reported for it.
    Error,
    /// End of input. Always the last token, with an empty span.
    Eof,
}

impl TokenKind {
    /// Returns true for tokens that never affect the meaning of a program.
    pub fn is_trivia(self) -> bool {
        matches!(
            self,
            Self::Whitespace | Self::Comment | Self::DocComment | Self::LineContinuation
        )
    }

    /// The keyword with the given spelling, if any.
    pub fn keyword(text: &str) -> Option<Self> {
        Some(match text {
            "and" => Self::KwAnd,
            "or" => Self::KwOr,
            "in" => Self::KwIn,
            "as" => Self::KwAs,
            "true" => Self::KwTrue,
            "false" => Self::KwFalse,
            "none" => Self::KwNone,
            "some" => Self::KwSome,
            "self" => Self::KwSelf,
            "Self" => Self::KwSelfType,
            "_" => Self::Underscore,
            _ => return None,
        })
    }

    /// A short human-readable description, for use in diagnostics.
    pub fn describe(self) -> &'static str {
        match self {
            Self::Whitespace => "whitespace",
            Self::Comment => "comment",
            Self::DocComment => "documentation comment",
            Self::LineContinuation => "line continuation",
            Self::Newline => "end of line",
            Self::Ident => "identifier",
            Self::Variable => "variable",
            Self::Underscore => "`_`",
            Self::KwAnd => "`and`",
            Self::KwOr => "`or`",
            Self::KwIn => "`in`",
            Self::KwAs => "`as`",
            Self::KwTrue => "`true`",
            Self::KwFalse => "`false`",
            Self::KwNone => "`none`",
            Self::KwSome => "`some`",
            Self::KwSelf => "`self`",
            Self::KwSelfType => "`Self`",
            Self::Int => "integer literal",
            Self::Float => "float literal",
            Self::Duration => "duration literal",
            Self::Char => "character literal",
            Self::RawString => "raw string literal",
            Self::StringStart => "string literal",
            Self::StringText => "string text",
            Self::StringEnd => "end of string",
            Self::InterpVar => "interpolated variable",
            Self::InterpParen => "`$(`",
            Self::InterpBracket => "`$[`",
            Self::LParen => "`(`",
            Self::RParen => "`)`",
            Self::LBracket => "`[`",
            Self::RBracket => "`]`",
            Self::LBrace => "`{`",
            Self::RBrace => "`}`",
            Self::Colon => "`:`",
            Self::Semi => "`;`",
            Self::Comma => "`,`",
            Self::Plus => "`+`",
            Self::Minus => "`-`",
            Self::Star => "`*`",
            Self::Slash => "`/`",
            Self::Percent => "`%`",
            Self::Bang => "`!`",
            Self::Tilde => "`~`",
            Self::Amp => "`&`",
            Self::Pipe => "`|`",
            Self::Caret => "`^`",
            Self::Dot => "`.`",
            Self::Eq => "`=`",
            Self::Ne => "`!=`",
            Self::Lt => "`<`",
            Self::Le => "`<=`",
            Self::Shl => "`<<`",
            Self::Gt => "`>`",
            Self::AndAnd => "`&&`",
            Self::OrOr => "`||`",
            Self::Arrow => "`->`",
            Self::Question => "`?`",
            Self::Error => "invalid token",
            Self::Eof => "end of file",
        }
    }
}

/// A token: its kind, its location, and whether it touches the previous token.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Token {
    /// What kind of token this is.
    pub kind: TokenKind,
    /// Where the token is in the source.
    pub span: Span,
    /// True if this token immediately follows the previous non-trivia token, with no whitespace,
    /// comment, line continuation or line break in between.
    ///
    /// The parser uses this flag for the "joined" rules of the language: command heads (`:put`),
    /// type annotations (`x:i64`), named arguments (`from=1`), paths (`/std/math`) and generic
    /// arguments (`List<i64>`). It is always false for the first token and for trivia.
    pub joined: bool,
}

impl Token {
    /// The source text of the token.
    pub fn text(self, source: &str) -> &str {
        &source[self.span.range()]
    }
}
