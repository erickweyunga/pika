//! Declarations: functions, types, imports, foreign functions and tests.

use pika_diagnostics::Diagnostic;

use super::exprs::{at_path, path};
use super::{
    ArgShape, arg, braced, default_value, form_args, name, start_form, type_annotation, types,
};
use crate::codes;
use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;
use crate::token::TokenKind;

/// Returns true at the start of a function parameter: `self`, `mut self`, `name:Type`,
/// `mut name:Type` or `owned name:Type`.
fn at_param_start(p: &Parser<'_>) -> bool {
    match p.current() {
        TokenKind::KwSelf => true,
        TokenKind::Ident
            if matches!(p.current_text(), "mut" | "owned")
                && matches!(p.nth(1), TokenKind::Ident | TokenKind::KwSelf) =>
        {
            true
        }
        // The colon must be joined; `type_annotation` reports it otherwise.
        TokenKind::Ident => p.nth_at(1, TokenKind::Colon),
        _ => false,
    }
}

/// `:fn name<T> params -> Ret raises do={...}`. The name is absent for anonymous functions
/// (`[:fn x:i64 -> i64 do={...}]`) and the body is absent for signatures in traits and
/// `:extern` blocks.
pub(super) fn fn_decl(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::FnDecl);
    if p.at(TokenKind::Ident) && !at_param_start(p) && !p.nth_at(1, TokenKind::Eq) {
        name(p);
        if p.at(TokenKind::Lt) && p.at_joined() {
            types::generic_params(p);
        }
    }
    p.start(SyntaxKind::ParamList);
    while at_param_start(p) {
        param(p);
    }
    p.finish_node();
    if p.at(TokenKind::Arrow) {
        p.start(SyntaxKind::RetType);
        p.bump();
        types::type_(p);
        p.finish_node();
    }
    if p.at_contextual("raises") {
        p.bump();
    }
    form_args(p, "fn", &[arg("do", ArgShape::Block, false)]);
    p.finish_node();
}

fn param(p: &mut Parser<'_>) {
    p.start(SyntaxKind::Param);
    if p.at_contextual("mut") || p.at_contextual("owned") {
        p.bump();
    }
    if p.at(TokenKind::KwSelf) {
        p.bump();
    } else {
        name(p);
        if p.at(TokenKind::Colon) {
            type_annotation(p);
        } else {
            p.error_expected("`:` and the parameter's type");
        }
        if p.at(TokenKind::Eq) && p.at_joined() {
            default_value(p);
        }
    }
    p.finish_node();
}

/// Name, generic parameters and `impl=` list shared by `:struct`, `:enum` and `:trait`.
fn type_decl_header(p: &mut Parser<'_>, command: &str) {
    name(p);
    if p.at(TokenKind::Lt) && p.at_joined() {
        types::generic_params(p);
    }
    form_args(p, command, &[arg("impl", ArgShape::ImplList, false)]);
}

/// `:struct Name<T> impl=A,B { fields and methods }`
pub(super) fn struct_decl(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::StructDecl);
    type_decl_header(p, "struct");
    member_list(p, field_decl);
    p.finish_node();
}

/// `:enum Name<T> impl=A,B { variants and methods }`
pub(super) fn enum_decl(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::EnumDecl);
    type_decl_header(p, "enum");
    member_list(p, variant_decl);
    p.finish_node();
}

/// `:trait Name<T> impl=Super { method signatures and default methods }`
pub(super) fn trait_decl(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::TraitDecl);
    type_decl_header(p, "trait");
    member_list(p, |p| p.error_expected("`:fn`"));
    p.finish_node();
}

/// `:impl<T> Type impl=Trait { methods and associated functions }`
pub(super) fn impl_decl(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::ImplDecl);
    if p.at(TokenKind::Lt) && p.at_joined() {
        types::generic_params(p);
    }
    if types::at_type_start(p) {
        types::type_(p);
    } else {
        p.error_expected("the type that the methods are for");
    }
    form_args(p, "impl", &[arg("impl", ArgShape::ImplList, false)]);
    member_list(p, |p| p.error_expected("`:fn`"));
    p.finish_node();
}

/// `:extern lib="c" { function signatures }`
pub(super) fn extern_decl(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::ExternDecl);
    form_args(p, "extern", &[arg("lib", ArgShape::Atom, true)]);
    member_list(p, |p| p.error_expected("`:fn`"));
    p.finish_node();
}

/// `{ members }` where each member is `:fn ...` or, for anything else, parsed by `other`.
fn member_list(p: &mut Parser<'_>, mut other: impl FnMut(&mut Parser<'_>)) {
    p.start(SyntaxKind::MemberList);
    braced(p, |p| {
        if p.at(TokenKind::Colon) && p.nth_text(1) == "fn" && p.nth_joined(1) {
            fn_decl(p);
        } else if p.at(TokenKind::Ident) {
            other(p);
        } else {
            p.error_expected("a member");
            p.bump_error();
        }
    });
    p.finish_node();
}

/// `name:Type` or `name:Type=default`
fn field_decl(p: &mut Parser<'_>) {
    p.start(SyntaxKind::FieldDecl);
    name(p);
    field_type(p);
    if p.at(TokenKind::Eq) && p.at_joined() {
        default_value(p);
    }
    p.finish_node();
}

/// The `:Type` of a field, with a hint when the colon is missing (`x i64`).
fn field_type(p: &mut Parser<'_>) {
    if p.at(TokenKind::Colon) {
        type_annotation(p);
        return;
    }
    let span = p.current_span();
    let mut diagnostic = Diagnostic::error(
        codes::EXPECTED,
        format!(
            "expected `:` and the field's type, found {}",
            p.current().describe()
        ),
        span,
    );
    if types::at_type_start(p) {
        diagnostic = diagnostic.with_help(format!(
            "write the type after a colon: `:{}`",
            p.current_text()
        ));
    }
    p.report(diagnostic);
}

/// `name field:Type field:Type ...`
fn variant_decl(p: &mut Parser<'_>) {
    p.start(SyntaxKind::VariantDecl);
    name(p);
    while p.at(TokenKind::Ident) {
        p.start(SyntaxKind::FieldDecl);
        name(p);
        field_type(p);
        p.finish_node();
    }
    p.finish_node();
}

/// `:use /std/math` or `:use /std/math as=m`
pub(super) fn use_decl(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::UseDecl);
    if at_path(p) {
        path(p);
    } else if p.at(TokenKind::Ident) {
        let name = p.current_text();
        let span = p.current_span();
        p.report(
            Diagnostic::error(
                codes::EXPECTED,
                format!("expected a module path, found `{name}`"),
                span,
            )
            .with_help(format!("module paths start with `/`, as in `/std/{name}`")),
        );
        p.bump_error();
    } else {
        p.error_expected("a module path such as `/std/math`");
    }
    form_args(p, "use", &[arg("as", ArgShape::Name, false)]);
    p.finish_node();
}

/// `:test "name" do={...}`
pub(super) fn test_decl(p: &mut Parser<'_>) {
    start_form(p, SyntaxKind::TestDecl);
    if p.at(TokenKind::StringStart) {
        super::atom(p);
    } else {
        p.error_expected("the test's name as a string");
    }
    form_args(p, "test", &[arg("do", ArgShape::Block, true)]);
    p.finish_node();
}
