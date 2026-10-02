//! Types, generic arguments and generic parameters.

use pika_diagnostics::Diagnostic;

use super::exprs::{at_path, path};
use crate::codes;
use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;
use crate::token::TokenKind;

/// Returns true at a token that can start a type.
pub(super) fn at_type_start(p: &Parser<'_>) -> bool {
    matches!(p.current(), TokenKind::Ident | TokenKind::KwSelfType) || at_path(p)
}

/// A type: `Name`, `Name<Args>`, `/module/Name`, `fn(A, B) -> R raises`, or any of these
/// followed by `?`.
pub(super) fn type_(p: &mut Parser<'_>) {
    let checkpoint = p.checkpoint();
    if p.at_contextual("fn") && p.nth_at(1, TokenKind::LParen) {
        fn_type(p);
    } else if at_type_start(p) {
        simple_path_type(p);
    } else {
        p.error_expected("a type");
        return;
    }
    while p.at(TokenKind::Question) && p.at_joined() {
        p.start_at(checkpoint, SyntaxKind::OptionType);
        p.bump();
        p.finish_node();
    }
}

/// `Name`, `Self`, `/module/Name`, each with optional joined generic arguments.
pub(super) fn simple_path_type(p: &mut Parser<'_>) {
    p.start(SyntaxKind::PathType);
    if p.at(TokenKind::Slash) {
        path(p);
    } else {
        p.start(SyntaxKind::NameRef);
        p.bump();
        p.finish_node();
    }
    if p.at(TokenKind::Lt) && p.at_joined() {
        generic_args(p);
    }
    p.finish_node();
}

/// `fn(A, B) -> R raises`
fn fn_type(p: &mut Parser<'_>) {
    p.start(SyntaxKind::FnType);
    p.bump(); // `fn`
    let opener = p.current_span();
    p.bump(); // `(`
    p.with_newlines(false, |p| {
        if !p.at(TokenKind::RParen) {
            type_(p);
            while p.eat(TokenKind::Comma) {
                type_(p);
            }
        }
        p.expect_closing(TokenKind::RParen, opener);
    });
    if p.at(TokenKind::Arrow) {
        p.start(SyntaxKind::RetType);
        p.bump();
        type_(p);
        p.finish_node();
    }
    if p.at_contextual("raises") {
        p.bump();
    }
    p.finish_node();
}

/// `<A, B>` after a type or command name.
///
/// If no type follows the `<`, nothing is consumed: this is most likely a comparison written
/// without spaces, as in `($a->len<3)`, which is reported and then parsed as one.
pub(crate) fn generic_args(p: &mut Parser<'_>) {
    let starts_type = matches!(
        p.nth(1),
        TokenKind::Ident | TokenKind::KwSelfType | TokenKind::Slash
    );
    if !starts_type {
        let found = p.nth(1).describe();
        let span = p.nth_span(1);
        p.report(
            Diagnostic::error(
                codes::EXPECTED,
                format!("expected a type argument after `<`, found {found}"),
                span,
            )
            .with_help(
                "a `<` joined to a name starts type arguments; to compare, put spaces around \
                 it: `a < b`",
            ),
        );
        return;
    }
    p.start(SyntaxKind::GenericArgList);
    let opener = p.current_span();
    p.bump(); // `<`
    p.with_newlines(false, |p| {
        type_(p);
        while p.eat(TokenKind::Comma) {
            type_(p);
        }
        p.expect_closing(TokenKind::Gt, opener);
    });
    p.finish_node();
}

/// `<T, U: Bound + Other>` after a declared name.
pub(super) fn generic_params(p: &mut Parser<'_>) {
    p.start(SyntaxKind::GenericParamList);
    let opener = p.current_span();
    p.bump(); // `<`
    p.with_newlines(false, |p| {
        loop {
            p.start(SyntaxKind::GenericParam);
            super::name(p);
            if p.eat(TokenKind::Colon) {
                type_(p);
                while p.eat(TokenKind::Plus) {
                    type_(p);
                }
            }
            p.finish_node();
            if !p.eat(TokenKind::Comma) {
                break;
            }
        }
        p.expect_closing(TokenKind::Gt, opener);
    });
    p.finish_node();
}

/// `Trait1,Trait2` after `impl=`.
pub(super) fn impl_list(p: &mut Parser<'_>) {
    p.start(SyntaxKind::ImplList);
    type_(p);
    while p.eat(TokenKind::Comma) {
        type_(p);
    }
    p.finish_node();
}
