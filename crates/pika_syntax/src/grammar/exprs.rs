//! Atoms (command arguments) and expressions (inside parentheses).

use pika_diagnostics::Diagnostic;

use super::{command, types};
use crate::codes;
use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;
use crate::token::TokenKind;

fn is_number(kind: TokenKind) -> bool {
    matches!(
        kind,
        TokenKind::Int | TokenKind::Float | TokenKind::Duration
    )
}

/// Returns true at `-` directly followed by a number, as in `-5`.
fn at_negative_number(p: &Parser<'_>) -> bool {
    p.at(TokenKind::Minus) && is_number(p.nth(1)) && p.nth_joined(1)
}

/// Returns true at `/` directly followed by a name: the start of a path such as `/std/math`.
pub(super) fn at_path(p: &Parser<'_>) -> bool {
    p.at(TokenKind::Slash) && p.nth_at(1, TokenKind::Ident) && p.nth_joined(1)
}

/// Returns true at a token that can start an atom.
pub(crate) fn at_atom_start(p: &Parser<'_>) -> bool {
    match p.current() {
        TokenKind::Int
        | TokenKind::Float
        | TokenKind::Duration
        | TokenKind::Char
        | TokenKind::RawString
        | TokenKind::StringStart
        | TokenKind::Variable
        | TokenKind::LParen
        | TokenKind::LBracket
        | TokenKind::LBrace
        | TokenKind::Ident
        | TokenKind::KwSelfType
        | TokenKind::KwTrue
        | TokenKind::KwFalse
        | TokenKind::KwNone => true,
        TokenKind::Slash => at_path(p),
        TokenKind::Minus => at_negative_number(p),
        _ => false,
    }
}

/// An atom: a literal, variable, path, parenthesized expression, command substitution,
/// collection or struct literal, or static member, followed by any number of `->` accesses.
///
/// Returns false, consuming nothing, if the current token cannot start an atom.
pub(crate) fn atom(p: &mut Parser<'_>) -> bool {
    if !at_atom_start(p) {
        return false;
    }
    let checkpoint = p.checkpoint();
    primary(p);
    // Member access is joined (`$p->x`); a spaced `->` is a return type, as in
    // `:fn f x:i64=0 -> i64`.
    while p.at(TokenKind::Arrow) && p.at_joined() {
        p.start_at(checkpoint, SyntaxKind::MemberExpr);
        p.bump();
        member(p);
        p.finish_node();
    }
    true
}

fn primary(p: &mut Parser<'_>) {
    match p.current() {
        TokenKind::Int
        | TokenKind::Float
        | TokenKind::Duration
        | TokenKind::Char
        | TokenKind::RawString
        | TokenKind::KwTrue
        | TokenKind::KwFalse
        | TokenKind::KwNone => {
            p.start(SyntaxKind::Literal);
            p.bump();
            p.finish_node();
        }
        TokenKind::Minus => {
            p.start(SyntaxKind::Literal);
            p.bump();
            p.bump();
            p.finish_node();
        }
        TokenKind::StringStart => string_lit(p),
        TokenKind::Variable => {
            p.start(SyntaxKind::VarExpr);
            p.bump();
            p.finish_node();
        }
        TokenKind::Slash => {
            // `/module/item`, or a type of another module in `/module/Type{...}`.
            let checkpoint = p.checkpoint();
            types::simple_path_type(p);
            let kind = if p.at(TokenKind::LBrace) && p.at_joined() {
                SyntaxKind::BraceLit
            } else {
                SyntaxKind::PathExpr
            };
            p.start_at(checkpoint, kind);
            if kind == SyntaxKind::BraceLit {
                brace_body(p);
            }
            p.finish_node();
        }
        TokenKind::LParen => paren_expr(p),
        TokenKind::LBracket => bracket_expr(p),
        TokenKind::LBrace => {
            p.start(SyntaxKind::BraceLit);
            brace_body(p);
            p.finish_node();
        }
        TokenKind::Ident | TokenKind::KwSelfType => type_head(p),
        kind => unreachable!("`at_atom_start` accepted {kind:?}"),
    }
}

/// A type name used as an atom: `Name{...}`, `Name<T>{...}` or `Name->member`.
fn type_head(p: &mut Parser<'_>) {
    let checkpoint = p.checkpoint();
    let name_text = p.current_text();
    let name_span = p.current_span();
    types::simple_path_type(p);
    if p.at(TokenKind::LBrace) && p.at_joined() {
        p.start_at(checkpoint, SyntaxKind::BraceLit);
        brace_body(p);
        p.finish_node();
    } else if !(p.at(TokenKind::Arrow) && p.at_joined()) {
        let starts_lowercase = name_text.starts_with(|c: char| c.is_ascii_lowercase());
        let help = if starts_lowercase {
            format!(
                "variables are read with `$`, as in `${name_text}`; strings need quotes, as in \"{name_text}\""
            )
        } else {
            format!(
                "use `{name_text}{{...}}` to build a value or `{name_text}->member` to access a member"
            )
        };
        p.report(
            Diagnostic::error(
                codes::TYPE_AS_VALUE,
                format!("`{name_text}` is not a value"),
                name_span,
            )
            .with_help(help),
        );
    }
}

/// The part after `->`: a field or method name, an index, or a key.
fn member(p: &mut Parser<'_>) {
    match p.current() {
        TokenKind::Ident => {
            p.start(SyntaxKind::NameRef);
            p.bump();
            p.finish_node();
            // A method's type arguments, as in `[$list->map<String> ...]`.
            if p.at(TokenKind::Lt) && p.at_joined() {
                types::generic_args(p);
            }
        }
        TokenKind::Int | TokenKind::Char | TokenKind::RawString => {
            p.start(SyntaxKind::Literal);
            p.bump();
            p.finish_node();
        }
        TokenKind::StringStart => string_lit(p),
        TokenKind::Variable => {
            p.start(SyntaxKind::VarExpr);
            p.bump();
            p.finish_node();
        }
        TokenKind::LParen => paren_expr(p),
        _ => p.error_expected("a field name, index or key after `->`"),
    }
}

/// `/name/name...` with every `/` and name joined.
pub(super) fn path(p: &mut Parser<'_>) {
    p.start(SyntaxKind::Path);
    p.bump(); // `/`
    if p.at(TokenKind::Ident) {
        p.bump();
    } else {
        p.error_expected("a module name after `/`");
    }
    while p.at(TokenKind::Slash) && p.at_joined() {
        if !p.nth_at(1, TokenKind::Ident) {
            break;
        }
        if !p.nth_joined(1) {
            let span = p.current_span().to(p.nth_span(1));
            p.report(Diagnostic::error(
                codes::PATH_SPACING,
                "paths must be written without spaces around `/`",
                span,
            ));
        }
        p.bump();
        p.bump();
    }
    p.finish_node();
}

/// `"text $var $(expr) $[command]"`
fn string_lit(p: &mut Parser<'_>) {
    p.start(SyntaxKind::StringLit);
    p.bump(); // opening quote
    loop {
        match p.current() {
            TokenKind::StringEnd => {
                p.bump();
                break;
            }
            TokenKind::Eof => break, // unterminated; reported by the lexer
            TokenKind::StringText | TokenKind::InterpVar | TokenKind::Error => p.bump(),
            TokenKind::InterpParen => {
                p.start(SyntaxKind::Interpolation);
                let opener = p.current_span();
                p.bump();
                p.with_newlines(false, |p| {
                    expr(p);
                    p.expect_closing(TokenKind::RParen, opener);
                });
                p.finish_node();
            }
            TokenKind::InterpBracket => {
                p.start(SyntaxKind::Interpolation);
                let opener = p.current_span();
                p.bump();
                p.with_newlines(false, |p| {
                    command(p);
                    p.expect_closing(TokenKind::RBracket, opener);
                });
                p.finish_node();
            }
            // Leftovers of a malformed interpolation; already reported.
            _ => p.bump_error(),
        }
    }
    p.finish_node();
}

/// `( expr )`
pub(super) fn paren_expr(p: &mut Parser<'_>) {
    p.start(SyntaxKind::ParenExpr);
    let opener = p.current_span();
    p.bump();
    p.with_newlines(false, |p| {
        expr(p);
        if p.at(TokenKind::Comma) {
            let span = p.current_span();
            p.report(
                Diagnostic::error(codes::TUPLES_RESERVED, "tuples are not supported yet", span)
                    .with_help("use a struct to group several values"),
            );
            while p.eat(TokenKind::Comma) {
                expr(p);
            }
        }
        p.expect_closing(TokenKind::RParen, opener);
    });
    p.finish_node();
}

/// A parenthesized condition, as required by `:if`, `:while` and `while=`.
pub(super) fn paren_condition(p: &mut Parser<'_>) {
    if p.at(TokenKind::LParen) {
        paren_expr(p);
        return;
    }
    let span = p.current_span();
    p.report(
        Diagnostic::error(codes::EXPECTED, "conditions must be in parentheses", span)
            .with_help("write the condition as `($x > 0)`"),
    );
    atom(p);
}

/// `[ command ]`
fn bracket_expr(p: &mut Parser<'_>) {
    p.start(SyntaxKind::BracketExpr);
    let opener = p.current_span();
    p.bump();
    p.with_newlines(false, |p| {
        command(p);
        p.expect_closing(TokenKind::RBracket, opener);
    });
    p.finish_node();
}

/// `{ elements }` of a collection or struct literal. Elements are separated by `;` or line
/// breaks and are values (`1`), map entries (`"key"=value`) or fields (`name=value`).
fn brace_body(p: &mut Parser<'_>) {
    let opener = p.current_span();
    if !p.expect(TokenKind::LBrace) {
        return;
    }
    p.with_newlines(true, |p| {
        loop {
            while p.at(TokenKind::Newline) || p.at(TokenKind::Semi) {
                p.bump();
            }
            if p.at(TokenKind::RBrace) || p.at(TokenKind::Eof) {
                break;
            }
            if !element(p) {
                p.error_expected("an element");
                p.bump_error();
                continue;
            }
            if !matches!(
                p.current(),
                TokenKind::Newline | TokenKind::Semi | TokenKind::RBrace
            ) {
                p.error_expected("`;` or a line break between elements");
            }
        }
        p.expect_closing(TokenKind::RBrace, opener);
    });
}

fn element(p: &mut Parser<'_>) -> bool {
    if p.at(TokenKind::Ident) && p.nth_at(1, TokenKind::Eq) {
        p.start(SyntaxKind::FieldInit);
        super::check_named_arg_spacing(p);
        p.start(SyntaxKind::NameRef);
        p.bump();
        p.finish_node();
        p.bump(); // `=`
        if !atom(p) {
            p.error_expected("a field value");
        }
        p.finish_node();
        return true;
    }
    let checkpoint = p.checkpoint();
    if !atom(p) {
        return false;
    }
    if p.at(TokenKind::Eq) {
        p.start_at(checkpoint, SyntaxKind::MapEntry);
        p.bump();
        if !atom(p) {
            p.error_expected("a map value");
        }
        p.finish_node();
    }
    true
}

// ----- Expressions ---------------------------------------------------------------------------

/// Binding power of prefix operators.
const PREFIX_BP: u8 = 23;
/// Binding power of `as`.
const CAST_BP: u8 = 21;
/// Left binding power of comparison operators, which are non-associative.
const COMPARISON_BP: u8 = 5;

/// A binary operator at the current position: how many tokens it spans and its left binding
/// power. Right binding power is one more (all binary operators associate to the left).
pub(super) fn binary_op_at(p: &Parser<'_>) -> Option<(usize, u8)> {
    let bp = match p.current() {
        TokenKind::KwOr | TokenKind::OrOr => 1,
        TokenKind::KwAnd | TokenKind::AndAnd => 3,
        TokenKind::Gt if p.nth_joined(1) && p.nth_at(1, TokenKind::Gt) => return Some((2, 15)),
        TokenKind::Gt if p.nth_joined(1) && p.nth_at(1, TokenKind::Eq) => {
            return Some((2, COMPARISON_BP));
        }
        TokenKind::Eq
        | TokenKind::Ne
        | TokenKind::Lt
        | TokenKind::Le
        | TokenKind::Gt
        | TokenKind::KwIn => COMPARISON_BP,
        TokenKind::Dot => 7,
        TokenKind::Pipe => 9,
        TokenKind::Caret => 11,
        TokenKind::Amp => 13,
        TokenKind::Shl => 15,
        TokenKind::Plus | TokenKind::Minus => 17,
        TokenKind::Star | TokenKind::Slash | TokenKind::Percent => 19,
        _ => return None,
    };
    Some((1, bp))
}

/// An expression inside parentheses.
pub(super) fn expr(p: &mut Parser<'_>) {
    expr_bp(p, 0);
}

fn expr_bp(p: &mut Parser<'_>, min_bp: u8) {
    let checkpoint = p.checkpoint();
    if matches!(
        p.current(),
        TokenKind::Minus | TokenKind::Bang | TokenKind::Tilde
    ) && !at_negative_number(p)
    {
        p.start(SyntaxKind::PrefixExpr);
        p.bump();
        expr_bp(p, PREFIX_BP);
        p.finish_node();
    } else if !atom(p) {
        p.error_expected("an expression");
        return;
    }

    let mut previous_was_comparison = false;
    loop {
        if p.at(TokenKind::KwAs) {
            if CAST_BP < min_bp {
                break;
            }
            p.start_at(checkpoint, SyntaxKind::CastExpr);
            p.bump();
            types::type_(p);
            p.finish_node();
            continue;
        }
        let Some((token_count, left_bp)) = binary_op_at(p) else {
            break;
        };
        if left_bp < min_bp {
            break;
        }
        let is_comparison = left_bp == COMPARISON_BP;
        if is_comparison && previous_was_comparison {
            let span = p.current_span();
            p.report(
                Diagnostic::error(
                    codes::CHAINED_COMPARISON,
                    "comparison operators cannot be chained",
                    span,
                )
                .with_help("combine comparisons with `and`, as in `($a < $b and $b < $c)`"),
            );
        }
        p.start_at(checkpoint, SyntaxKind::BinExpr);
        for _ in 0..token_count {
            p.bump();
        }
        expr_bp(p, left_bp + 1);
        p.finish_node();
        previous_was_comparison = is_comparison;
    }
}
