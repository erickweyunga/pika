//! Built-in statement forms: declarations of variables, assignment, control flow.

use pika_diagnostics::Diagnostic;

use super::exprs::{paren_condition, paren_expr};
use super::{
    ArgShape, arg, atom, block, braced, form_args, name, patterns, start_form, type_annotation,
};
use crate::codes;
use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;
use crate::token::TokenKind;

/// `:local name:Type value`, `:const name value`, `:global NAME:Type value`
pub(super) fn var_decl(p: &mut Parser<'_>, kind: SyntaxKind) {
    start_form(p, kind);
    name(p);
    if p.at(TokenKind::Colon) {
        type_annotation(p);
    }
    atom(p);
    p.finish_node();
}

/// `:set name value` or `:set ($place->field) value`
pub(super) fn set_stmt(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::SetStmt);
    match p.current() {
        TokenKind::Ident => {
            p.start(SyntaxKind::NameRef);
            p.bump();
            p.finish_node();
        }
        TokenKind::LParen => paren_expr(p),
        TokenKind::Variable => {
            let text = p.current_text();
            let span = p.current_span();
            p.report(
                Diagnostic::error(
                    codes::DOLLAR_IN_DECLARATION,
                    format!("`:set` takes the name of the variable, not its value `{text}`"),
                    span,
                )
                .with_help(format!(
                    "write `:set {} ...`; to assign to a field or element, use parentheses: \
                     `:set ({text}->field) ...`",
                    &text[1..]
                )),
            );
            atom(p);
        }
        _ => p.error_expected("a variable name or a parenthesized place to assign to"),
    }
    if !atom(p) {
        p.error_expected("the value to assign");
    }
    p.finish_node();
}

/// `:if (cond) do={...} else={...}`
pub(super) fn if_stmt(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::IfStmt);
    paren_condition(p);
    form_args(
        p,
        "if",
        &[
            arg("do", ArgShape::Block, true),
            arg("else", ArgShape::BlockOrIf, false),
        ],
    );
    p.finish_node();
}

/// `:while (cond) do={...}`
pub(super) fn while_stmt(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::WhileStmt);
    paren_condition(p);
    form_args(p, "while", &[arg("do", ArgShape::Block, true)]);
    p.finish_node();
}

/// `:do {...} while=(cond)`
pub(super) fn do_while_stmt(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::DoWhileStmt);
    block(p);
    form_args(p, "do", &[arg("while", ArgShape::Paren, true)]);
    p.finish_node();
}

/// `:for i from=a to=b step=c do={...}` (or `until=b`)
pub(super) fn for_stmt(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::ForStmt);
    name(p);
    form_args(
        p,
        "for",
        &[
            arg("from", ArgShape::Atom, true),
            arg("to", ArgShape::Atom, false),
            arg("until", ArgShape::Atom, false),
            arg("step", ArgShape::Atom, false),
            arg("do", ArgShape::Block, true),
        ],
    );
    p.finish_node();
}

/// `:foreach x in=$items do={...}`, `:foreach k,v in=...`, `:foreach mut x in=...`
pub(super) fn foreach_stmt(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::ForeachStmt);
    if p.at_contextual("mut") && p.nth_at(1, TokenKind::Ident) {
        p.bump();
    }
    name(p);
    if p.eat(TokenKind::Comma) {
        name(p);
    }
    form_args(
        p,
        "foreach",
        &[
            arg("in", ArgShape::Atom, true),
            arg("do", ArgShape::Block, true),
        ],
    );
    p.finish_node();
}

/// `:match $value { Pattern do={...} ... }`
pub(super) fn match_stmt(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::MatchStmt);
    if !atom(p) {
        p.error_expected("a value to match on");
    }
    p.start(SyntaxKind::MatchArmList);
    braced(p, match_arm);
    p.finish_node();
    p.finish_node();
}

/// `Pattern if=(guard) do={...}`
fn match_arm(p: &mut Parser<'_>) {
    p.start(SyntaxKind::MatchArm);
    patterns::pattern(p);
    form_args(
        p,
        "match",
        &[
            arg("if", ArgShape::Paren, false),
            arg("do", ArgShape::Block, true),
        ],
    );
    p.finish_node();
}

/// `:onerror e in={...} do={...}`
pub(super) fn onerror_stmt(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::OnErrorStmt);
    name(p);
    form_args(
        p,
        "onerror",
        &[
            arg("in", ArgShape::Block, true),
            arg("do", ArgShape::Block, true),
        ],
    );
    p.finish_node();
}

/// `:unsafe {...}`
pub(super) fn unsafe_block(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::UnsafeBlock);
    block(p);
    p.finish_node();
}
