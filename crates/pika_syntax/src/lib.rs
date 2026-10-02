//! Lexer and parser for the Pika programming language.
//!
//! [`parse`] turns source text into a lossless concrete syntax tree: every byte of the input,
//! including whitespace and comments, is preserved, so the tree can drive a formatter and an
//! editor integration as well as the compiler.

pub mod ast;
pub mod codes;
mod grammar;
pub mod lexer;
pub mod literal;
mod parser;
pub mod syntax_kind;
pub mod token;

use std::fmt::Write;

pub use lexer::{Lexed, is_identifier, lex};
pub use parser::{Parse, parse, parse_at};
pub use syntax_kind::{PikaLanguage, SyntaxElement, SyntaxKind, SyntaxNode, SyntaxToken};
pub use token::{Token, TokenKind};

/// Formats tokens one per line as `Kind start..end "text"`, for debugging and snapshot tests.
///
/// Joined tokens are marked with `+`. Trivia is included only if `include_trivia` is true.
pub fn dump_tokens(source: &str, tokens: &[Token], include_trivia: bool) -> String {
    let mut out = String::new();
    for token in tokens {
        if token.kind.is_trivia() && !include_trivia {
            continue;
        }
        let marker = if token.joined { "+" } else { " " };
        writeln!(
            out,
            "{marker}{:?} {:?} {:?}",
            token.kind,
            token.span,
            token.text(source)
        )
        .expect("writing to a String cannot fail");
    }
    out
}
