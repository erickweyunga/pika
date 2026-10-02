//! The lexer: turns source text into a lossless stream of tokens.
//!
//! See section 3 of the language specification. The lexer is context-free except for string
//! literals: inside a string it switches to string mode, and `$(` / `$[` switch back to code
//! mode until the matching `)` / `]`. Context-dependent decisions (which `:` starts a command,
//! whether a line break ends a statement, how `>` tokens combine) are left to the parser, which
//! uses [`Token::joined`].

use pika_diagnostics::{Diagnostic, Span};

use crate::codes;
use crate::literal::{self, LiteralError};
use crate::token::{Token, TokenKind};

/// The result of lexing a source file.
#[derive(Clone, Debug, Default)]
pub struct Lexed {
    /// All tokens, including trivia, ending with [`TokenKind::Eof`].
    pub tokens: Vec<Token>,
    /// Problems found while lexing, in source order.
    pub diagnostics: Vec<Diagnostic>,
}

/// Returns true if `text` is an identifier that is not a keyword: a name that paths and
/// declarations can use, such as the name of a package or module.
pub fn is_identifier(text: &str) -> bool {
    let lexed = lex(text);
    matches!(
        lexed.tokens.as_slice(),
        [ident, eof] if ident.kind == TokenKind::Ident
            && ident.span == Span::new(0, u32::try_from(text.len()).unwrap_or(u32::MAX))
            && eof.kind == TokenKind::Eof
    ) && lexed.diagnostics.is_empty()
}

/// Lexes a whole source file.
///
/// The returned tokens cover every byte of `source` exactly once, in order, followed by an empty
/// [`TokenKind::Eof`] token. Lexing never fails: invalid input produces [`TokenKind::Error`]
/// tokens or diagnostics, and lexing continues.
pub fn lex(source: &str) -> Lexed {
    if u32::try_from(source.len()).is_err() {
        return Lexed {
            tokens: Vec::new(),
            diagnostics: vec![Diagnostic::error(
                codes::FILE_TOO_LARGE,
                "source file is larger than 4 GiB",
                Span::empty(0),
            )],
        };
    }
    let mut lexer = Lexer::new(source);
    lexer.run();
    Lexed {
        tokens: lexer.tokens,
        diagnostics: lexer.diagnostics,
    }
}

/// What the lexer is currently inside of.
enum Frame {
    /// Ordinary code: the top level, or an interpolation inside a string.
    Code {
        /// For an interpolation, the token that ends it (`)` or `]`) and the offset of its `$`.
        interpolation: Option<(TokenKind, usize)>,
        /// Closing delimiters expected for the brackets opened inside this frame.
        open: Vec<TokenKind>,
    },
    /// The inside of a string literal whose opening quote is at `start`.
    Str { start: usize },
}

struct Lexer<'s> {
    source: &'s str,
    bytes: &'s [u8],
    pos: usize,
    tokens: Vec<Token>,
    diagnostics: Vec<Diagnostic>,
    frames: Vec<Frame>,
}

fn is_ident_start(b: u8) -> bool {
    b.is_ascii_alphabetic() || b == b'_'
}

fn is_ident_continue(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b == b'_'
}

/// Converts a byte offset to `u32`. Offsets fit because `lex` rejects files over 4 GiB.
fn offset(at: usize) -> u32 {
    u32::try_from(at).expect("source length checked in `lex`")
}

fn span(start: usize, end: usize) -> Span {
    Span::new(offset(start), offset(end))
}

impl<'s> Lexer<'s> {
    fn new(source: &'s str) -> Self {
        Self {
            source,
            bytes: source.as_bytes(),
            pos: 0,
            tokens: Vec::new(),
            diagnostics: Vec::new(),
            frames: vec![Frame::Code {
                interpolation: None,
                open: Vec::new(),
            }],
        }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn peek_at(&self, ahead: usize) -> Option<u8> {
        self.bytes.get(self.pos + ahead).copied()
    }

    fn eat_while(&mut self, predicate: impl Fn(u8) -> bool) {
        while self.peek().is_some_and(&predicate) {
            self.pos += 1;
        }
    }

    /// Length in bytes of the line break at the current position, if there is one.
    fn line_break_len(&self, at: usize) -> Option<usize> {
        match self.bytes.get(at) {
            Some(b'\r') if self.bytes.get(at + 1) == Some(&b'\n') => Some(2),
            Some(b'\n' | b'\r') => Some(1),
            _ => None,
        }
    }

    /// Length in bytes of the (possibly multi-byte) character at `at`.
    fn char_len(&self, at: usize) -> usize {
        self.source[at..].chars().next().map_or(1, char::len_utf8)
    }

    fn push(&mut self, kind: TokenKind, start: usize) {
        let joined = !kind.is_trivia()
            && self
                .tokens
                .last()
                .is_some_and(|prev| !prev.kind.is_trivia() && prev.kind != TokenKind::Newline);
        self.tokens.push(Token {
            kind,
            span: span(start, self.pos),
            joined,
        });
    }

    fn error(&mut self, diagnostic: Diagnostic) {
        self.diagnostics.push(diagnostic);
    }

    fn literal_error(&mut self, base: usize, error: LiteralError) {
        let mut diagnostic = Diagnostic::error(
            error.code,
            error.message,
            span(base + error.range.start, base + error.range.end),
        );
        diagnostic.help = error.help;
        self.error(diagnostic);
    }

    fn run(&mut self) {
        if self.source.starts_with('\u{FEFF}') {
            self.pos = 3;
            self.push(TokenKind::Whitespace, 0);
        }
        while self.pos < self.bytes.len() {
            if matches!(self.frames.last(), Some(Frame::Str { .. })) {
                self.string_piece();
            } else {
                self.code_token();
            }
        }
        self.report_unterminated_string();
        self.push(TokenKind::Eof, self.pos);
        self.tokens.last_mut().expect("just pushed").joined = false;
    }

    fn report_unterminated_string(&mut self) {
        let mut interpolation = None;
        let innermost = self.frames.iter().rev().find_map(|frame| match frame {
            Frame::Str { start } => Some(*start),
            Frame::Code {
                interpolation: Some((_, at)),
                ..
            } => {
                interpolation.get_or_insert(*at);
                None
            }
            Frame::Code { .. } => None,
        });
        if let Some(start) = innermost {
            let mut diagnostic = Diagnostic::error(
                codes::UNTERMINATED_STRING,
                "unterminated string",
                span(start, start + 1),
            )
            .with_label("string starts here");
            if let Some(at) = interpolation {
                diagnostic = diagnostic
                    .with_secondary(span(at, at + 2), "this interpolation is never closed");
            } else {
                diagnostic = diagnostic.with_help("add a closing `\"`");
            }
            self.error(diagnostic);
        }
    }

    // ----- Code mode ---------------------------------------------------------------------------

    fn code_token(&mut self) {
        let start = self.pos;
        let b = self.bytes[self.pos];
        let kind = match b {
            b' ' | b'\t' => {
                self.eat_while(|b| b == b' ' || b == b'\t');
                TokenKind::Whitespace
            }
            b'\n' | b'\r' => {
                self.pos += self.line_break_len(start).expect("at a line break");
                TokenKind::Newline
            }
            b'#' => {
                self.eat_while(|b| b != b'\n' && b != b'\r');
                if self.bytes.get(start + 1) == Some(&b'#') {
                    TokenKind::DocComment
                } else {
                    TokenKind::Comment
                }
            }
            b'\\' => self.backslash(),
            b'"' => {
                self.pos += 1;
                self.frames.push(Frame::Str { start });
                TokenKind::StringStart
            }
            b'\'' => self.char_literal(),
            b'$' => self.variable(),
            b'0'..=b'9' => self.number(),
            b'(' => self.open_delim(TokenKind::LParen, TokenKind::RParen),
            b'[' => self.open_delim(TokenKind::LBracket, TokenKind::RBracket),
            b'{' => self.open_delim(TokenKind::LBrace, TokenKind::RBrace),
            b')' => self.close_delim(TokenKind::RParen),
            b']' => self.close_delim(TokenKind::RBracket),
            b'}' => self.close_delim(TokenKind::RBrace),
            b'=' if self.peek_at(1) == Some(b'=') => {
                self.pos += 2;
                self.error(
                    Diagnostic::error(
                        codes::DOUBLE_EQUALS,
                        "`==` is not an operator",
                        span(start, self.pos),
                    )
                    .with_help("use `=` for equality, for example `($a = $b)`"),
                );
                TokenKind::Eq
            }
            _ if is_ident_start(b) => self.ident_or_keyword(),
            _ if !b.is_ascii() => self.non_ascii(),
            _ => self.operator(),
        };
        self.push(kind, start);
    }

    fn operator(&mut self) -> TokenKind {
        let b = self.bytes[self.pos];
        let next = self.peek_at(1);
        let (kind, len) = match (b, next) {
            (b'-', Some(b'>')) => (TokenKind::Arrow, 2),
            (b'!', Some(b'=')) => (TokenKind::Ne, 2),
            (b'&', Some(b'&')) => (TokenKind::AndAnd, 2),
            (b'|', Some(b'|')) => (TokenKind::OrOr, 2),
            (b'<', Some(b'=')) => (TokenKind::Le, 2),
            (b'<', Some(b'<')) => (TokenKind::Shl, 2),
            (b':', _) => (TokenKind::Colon, 1),
            (b';', _) => (TokenKind::Semi, 1),
            (b',', _) => (TokenKind::Comma, 1),
            (b'+', _) => (TokenKind::Plus, 1),
            (b'-', _) => (TokenKind::Minus, 1),
            (b'*', _) => (TokenKind::Star, 1),
            (b'/', _) => (TokenKind::Slash, 1),
            (b'%', _) => (TokenKind::Percent, 1),
            (b'!', _) => (TokenKind::Bang, 1),
            (b'~', _) => (TokenKind::Tilde, 1),
            (b'&', _) => (TokenKind::Amp, 1),
            (b'|', _) => (TokenKind::Pipe, 1),
            (b'^', _) => (TokenKind::Caret, 1),
            (b'.', _) => (TokenKind::Dot, 1),
            (b'=', _) => (TokenKind::Eq, 1),
            (b'<', _) => (TokenKind::Lt, 1),
            (b'>', _) => (TokenKind::Gt, 1),
            (b'?', _) => (TokenKind::Question, 1),
            _ => {
                let start = self.pos;
                self.pos += 1;
                let message = match b {
                    b'`' => "unexpected character `` ` ``".to_owned(),
                    _ if b.is_ascii_graphic() => {
                        format!("unexpected character `{}`", char::from(b))
                    }
                    _ => format!("unexpected control character U+{b:04X}"),
                };
                self.error(Diagnostic::error(
                    codes::UNEXPECTED_CHARACTER,
                    message,
                    span(start, self.pos),
                ));
                return TokenKind::Error;
            }
        };
        self.pos += len;
        kind
    }

    fn open_delim(&mut self, kind: TokenKind, closer: TokenKind) -> TokenKind {
        self.pos += 1;
        if let Some(Frame::Code { open, .. }) = self.frames.last_mut() {
            open.push(closer);
        }
        kind
    }

    fn close_delim(&mut self, kind: TokenKind) -> TokenKind {
        self.pos += 1;
        let Some(Frame::Code {
            interpolation,
            open,
        }) = self.frames.last_mut()
        else {
            unreachable!("delimiters are only lexed in code mode");
        };
        if let Some(index) = open.iter().rposition(|&closer| closer == kind) {
            // Close this bracket and any unclosed brackets inside it. Bracket mismatches are
            // reported by the parser; the lexer only needs to find where interpolations end.
            open.truncate(index);
        } else if open.is_empty() && interpolation.is_some() {
            // Any closing delimiter ends an interpolation. A wrong one, as in `"$(a]"`, is
            // reported by the parser.
            self.frames.pop();
        }
        kind
    }

    fn backslash(&mut self) -> TokenKind {
        let start = self.pos;
        self.pos += 1;
        if let Some(len) = self.line_break_len(self.pos) {
            self.pos += len;
            return TokenKind::LineContinuation;
        }
        let message = if self.pos == self.bytes.len() {
            "backslash at end of file"
        } else {
            "a backslash outside a string must be the last character on its line"
        };
        self.error(
            Diagnostic::error(codes::STRAY_BACKSLASH, message, span(start, self.pos)).with_help(
                "a line ending in `\\` continues on the next line, and cannot carry a comment",
            ),
        );
        TokenKind::Error
    }

    fn variable(&mut self) -> TokenKind {
        let start = self.pos;
        self.pos += 1;
        if self.peek().is_some_and(is_ident_start) {
            self.eat_while(is_ident_continue);
            return TokenKind::Variable;
        }
        self.error(
            Diagnostic::error(
                codes::STRAY_DOLLAR,
                "expected a variable name after `$`",
                span(start, self.pos),
            )
            .with_help("variables are read with `$name`"),
        );
        TokenKind::Error
    }

    fn ident_or_keyword(&mut self) -> TokenKind {
        let start = self.pos;
        self.eat_while(is_ident_continue);
        if self.peek().is_some_and(|b| !b.is_ascii()) && self.next_char_is_alphanumeric() {
            return self.finish_non_ascii_ident(start);
        }
        let text = &self.source[start..self.pos];
        if text == "r"
            && let Some(kind) = self.raw_string(start)
        {
            return kind;
        }
        TokenKind::keyword(text).unwrap_or(TokenKind::Ident)
    }

    fn next_char_is_alphanumeric(&self) -> bool {
        self.source[self.pos..]
            .chars()
            .next()
            .is_some_and(char::is_alphanumeric)
    }

    fn non_ascii(&mut self) -> TokenKind {
        let start = self.pos;
        if self.next_char_is_alphanumeric() {
            return self.finish_non_ascii_ident(start);
        }
        let c = self.source[start..].chars().next().expect("not at end");
        self.pos += c.len_utf8();
        self.error(Diagnostic::error(
            codes::UNEXPECTED_CHARACTER,
            format!("unexpected character `{c}` (U+{:04X})", u32::from(c)),
            span(start, self.pos),
        ));
        TokenKind::Error
    }

    /// Consumes the rest of an identifier-like word containing non-ASCII letters.
    fn finish_non_ascii_ident(&mut self, start: usize) -> TokenKind {
        let rest = &self.source[self.pos..];
        let len = rest
            .find(|c: char| !(c.is_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        self.pos += len;
        self.error(
            Diagnostic::error(
                codes::NON_ASCII_IDENT,
                format!(
                    "identifier `{}` contains non-ASCII characters",
                    &self.source[start..self.pos]
                ),
                span(start, self.pos),
            )
            .with_help("identifiers may only use ASCII letters, digits and `_`"),
        );
        TokenKind::Error
    }

    /// Lexes a raw string if the `r` just consumed starts one (`r"` or `r#...#"`).
    fn raw_string(&mut self, start: usize) -> Option<TokenKind> {
        let hashes = self.bytes[self.pos..]
            .iter()
            .take_while(|&&b| b == b'#')
            .count();
        if self.bytes.get(self.pos + hashes) != Some(&b'"') {
            return None;
        }
        self.pos += hashes + 1;
        let closing = format!("\"{}", "#".repeat(hashes));
        if let Some(index) = self.source[self.pos..].find(&closing) {
            self.pos += index + closing.len();
        } else {
            self.pos = self.bytes.len();
            self.error(
                Diagnostic::error(
                    codes::UNTERMINATED_RAW_STRING,
                    "unterminated raw string",
                    span(start, start + 2 + hashes),
                )
                .with_label("raw string starts here")
                .with_help(format!("add a closing `{closing}`")),
            );
        }
        Some(TokenKind::RawString)
    }

    fn char_literal(&mut self) -> TokenKind {
        let start = self.pos;
        self.pos += 1;
        loop {
            match self.peek() {
                None | Some(b'\n' | b'\r') => {
                    self.error(
                        Diagnostic::error(
                            codes::UNTERMINATED_CHAR,
                            "unterminated character literal",
                            span(start, self.pos),
                        )
                        .with_help("add a closing `'`"),
                    );
                    return TokenKind::Char;
                }
                Some(b'\'') => {
                    self.pos += 1;
                    break;
                }
                Some(b'\\') => {
                    self.pos += 1;
                    if self.peek().is_some_and(|b| b != b'\n' && b != b'\r') {
                        self.pos += self.char_len(self.pos);
                    }
                }
                Some(_) => self.pos += self.char_len(self.pos),
            }
        }
        if let Err(error) = literal::parse_char(&self.source[start..self.pos]) {
            self.literal_error(start, error);
        }
        TokenKind::Char
    }

    fn number(&mut self) -> TokenKind {
        let start = self.pos;
        if self.bytes[start] == b'0' && matches!(self.peek_at(1), Some(b'x' | b'o' | b'b')) {
            self.pos += 2;
            self.eat_while(is_ident_continue);
            if let Err(error) = literal::parse_int(&self.source[start..self.pos]) {
                self.literal_error(start, error);
            }
            return TokenKind::Int;
        }

        self.eat_while(|b| b.is_ascii_digit() || b == b'_');
        let mut is_float = false;
        if self.peek() == Some(b'.') && self.peek_at(1).is_some_and(|b| b.is_ascii_digit()) {
            self.pos += 1;
            self.eat_while(|b| b.is_ascii_digit() || b == b'_');
            is_float = true;
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            let exponent_digits_at = match self.peek_at(1) {
                Some(b'+' | b'-') => 2,
                _ => 1,
            };
            if self
                .peek_at(exponent_digits_at)
                .is_some_and(|b| b.is_ascii_digit())
            {
                self.pos += exponent_digits_at;
                self.eat_while(|b| b.is_ascii_digit() || b == b'_');
                is_float = true;
            }
        }

        if is_float {
            let number_end = self.pos;
            self.eat_while(is_ident_continue);
            if self.pos > number_end {
                let suffix = &self.source[number_end..self.pos];
                let help = if literal::starts_with_duration_unit(suffix) {
                    "duration literals must be whole numbers, for example `1500ms` instead of `1.5s`"
                } else {
                    "number literals have no type suffixes; convert with `as`, for example `(1.5 as f32)`"
                };
                self.error(
                    Diagnostic::error(
                        codes::INVALID_SUFFIX,
                        format!("invalid suffix `{suffix}` on float literal"),
                        span(number_end, self.pos),
                    )
                    .with_help(help),
                );
            } else if let Err(error) = literal::parse_float(&self.source[start..self.pos]) {
                self.literal_error(start, error);
            }
            return TokenKind::Float;
        }

        if self.peek().is_some_and(|b| b.is_ascii_alphabetic()) {
            self.eat_while(is_ident_continue);
            return match literal::parse_duration(&self.source[start..self.pos]) {
                Ok(_) => TokenKind::Duration,
                Err(error) => {
                    let kind = if error.code == codes::INVALID_SUFFIX {
                        TokenKind::Int
                    } else {
                        TokenKind::Duration
                    };
                    self.literal_error(start, error);
                    kind
                }
            };
        }

        if let Err(error) = literal::parse_int(&self.source[start..self.pos]) {
            self.literal_error(start, error);
        }
        TokenKind::Int
    }

    // ----- String mode -------------------------------------------------------------------------

    fn string_piece(&mut self) {
        let start = self.pos;
        match self.bytes[self.pos] {
            b'"' => {
                self.pos += 1;
                self.frames.pop();
                self.push(TokenKind::StringEnd, start);
            }
            b'$' => {
                self.pos += 1;
                let kind = match self.peek() {
                    Some(b'(') => {
                        self.pos += 1;
                        self.frames.push(Frame::Code {
                            interpolation: Some((TokenKind::RParen, start)),
                            open: Vec::new(),
                        });
                        TokenKind::InterpParen
                    }
                    Some(b'[') => {
                        self.pos += 1;
                        self.frames.push(Frame::Code {
                            interpolation: Some((TokenKind::RBracket, start)),
                            open: Vec::new(),
                        });
                        TokenKind::InterpBracket
                    }
                    Some(b) if is_ident_start(b) => {
                        self.eat_while(is_ident_continue);
                        TokenKind::InterpVar
                    }
                    _ => {
                        self.error(
                            Diagnostic::error(
                                codes::STRAY_DOLLAR,
                                "expected a variable name, `(` or `[` after `$`",
                                span(start, self.pos),
                            )
                            .with_help("write `\\$` for a literal dollar sign"),
                        );
                        TokenKind::Error
                    }
                };
                self.push(kind, start);
            }
            _ => {
                while let Some(b) = self.peek() {
                    match b {
                        b'"' | b'$' => break,
                        b'\\' => {
                            self.pos += 1;
                            if self.pos < self.bytes.len() {
                                self.pos += self
                                    .line_break_len(self.pos)
                                    .unwrap_or_else(|| self.char_len(self.pos));
                            }
                        }
                        _ => self.pos += self.char_len(self.pos),
                    }
                }
                let (_, errors) = literal::unescape(&self.source[start..self.pos]);
                for error in errors {
                    self.literal_error(start, error);
                }
                self.push(TokenKind::StringText, start);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::is_identifier;

    #[test]
    fn identifiers() {
        for name in ["std", "net_http", "_private", "Point2"] {
            assert!(is_identifier(name), "{name}");
        }
        for name in ["", "2d", "my-package", "true", "none", "a b", "x$", "self"] {
            assert!(!is_identifier(name), "{name}");
        }
    }
}
