//! Patterns in `:match` arms.

use super::exprs::at_path;
use super::types;
use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;
use crate::token::TokenKind;

/// Returns true at a token that can start a pattern, excluding `name=` (the start of an arm's
/// `do=` or `if=` argument).
fn at_pattern_start(p: &Parser<'_>) -> bool {
    match p.current() {
        TokenKind::Underscore
        | TokenKind::Int
        | TokenKind::Float
        | TokenKind::Duration
        | TokenKind::Char
        | TokenKind::RawString
        | TokenKind::StringStart
        | TokenKind::KwTrue
        | TokenKind::KwFalse
        | TokenKind::KwNone
        | TokenKind::KwSome
        | TokenKind::LParen
        | TokenKind::KwSelfType => true,
        TokenKind::Ident => !p.nth_at(1, TokenKind::Eq),
        TokenKind::Minus => {
            matches!(
                p.nth(1),
                TokenKind::Int | TokenKind::Float | TokenKind::Duration
            ) && p.nth_joined(1)
        }
        TokenKind::Slash => at_path(p),
        _ => false,
    }
}

/// Returns true at `Type->` or `Type<`: the start of a variant pattern rather than a binding.
fn at_variant_head(p: &Parser<'_>) -> bool {
    p.nth_joined(1) && matches!(p.nth(1), TokenKind::Arrow | TokenKind::Lt)
}

/// A pattern, including variant patterns with sub-patterns: `Shape->circle _ r`.
pub(super) fn pattern(p: &mut Parser<'_>) {
    pattern_inner(p, true);
}

fn pattern_inner(p: &mut Parser<'_>, allow_variant_fields: bool) {
    if !at_pattern_start(p) {
        p.error_expected("a pattern");
        let at_boundary = matches!(
            p.current(),
            TokenKind::Newline | TokenKind::Semi | TokenKind::RBrace | TokenKind::Eof
        ) || super::at_named_arg(p);
        if !at_boundary {
            p.bump_error();
        }
        return;
    }
    match p.current() {
        TokenKind::Underscore => {
            p.start(SyntaxKind::WildcardPat);
            p.bump();
            p.finish_node();
        }
        TokenKind::KwNone => {
            p.start(SyntaxKind::NonePat);
            p.bump();
            p.finish_node();
        }
        TokenKind::KwSome => {
            p.start(SyntaxKind::SomePat);
            p.bump();
            pattern_inner(p, false);
            p.finish_node();
        }
        TokenKind::LParen => {
            p.start(SyntaxKind::ParenPat);
            let opener = p.current_span();
            p.bump();
            p.with_newlines(false, |p| {
                pattern(p);
                p.expect_closing(TokenKind::RParen, opener);
            });
            p.finish_node();
        }
        TokenKind::StringStart => {
            p.start(SyntaxKind::LiteralPat);
            super::atom(p);
            p.finish_node();
        }
        TokenKind::Minus => {
            p.start(SyntaxKind::LiteralPat);
            p.bump();
            p.bump();
            p.finish_node();
        }
        TokenKind::Ident if !at_variant_head(p) => {
            p.start(SyntaxKind::BindingPat);
            super::name(p);
            p.finish_node();
        }
        TokenKind::Ident | TokenKind::KwSelfType | TokenKind::Slash => {
            variant_pattern(p, allow_variant_fields);
        }
        _ => {
            p.start(SyntaxKind::LiteralPat);
            p.bump();
            p.finish_node();
        }
    }
}

/// `Type->variant p1 p2 ...`. Nested variant patterns with fields need parentheses.
fn variant_pattern(p: &mut Parser<'_>, allow_fields: bool) {
    p.start(SyntaxKind::VariantPat);
    types::simple_path_type(p);
    if p.expect(TokenKind::Arrow) {
        if p.at(TokenKind::Ident) && !p.nth_at(1, TokenKind::Eq) {
            p.start(SyntaxKind::NameRef);
            p.bump();
            p.finish_node();
        } else {
            p.error_expected("a variant name");
        }
    }
    if allow_fields {
        while at_pattern_start(p) {
            pattern_inner(p, false);
        }
    }
    p.finish_node();
}
