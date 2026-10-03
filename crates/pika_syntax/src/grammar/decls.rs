//! Declarations: functions, types, imports, foreign functions and tests.

use pika_diagnostics::{Diagnostic, Span};

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
    let mut decl = TypeDecl::new(p);
    start_form(p, SyntaxKind::StructDecl);
    type_decl_header(p, "struct");
    member_list(p, |p| {
        // `circle radius:f64`: a variant, written in a struct.
        let variant = p.nth_at(1, TokenKind::Ident)
            && !p.nth_joined(1)
            && p.nth_at(2, TokenKind::Colon)
            && p.nth_joined(2);
        if variant {
            let start = p.current_span().start;
            variant_decl(p);
            decl.report_other_kind(p, start);
        } else {
            field_decl(p);
        }
    });
    p.finish_node();
}

/// `:enum Name<T> impl=A,B { variants and methods }`
pub(super) fn enum_decl(p: &mut Parser<'_>) {
    let mut decl = TypeDecl::new(p);
    start_form(p, SyntaxKind::EnumDecl);
    type_decl_header(p, "enum");
    member_list(p, |p| {
        // `x:f64`: a field, written in an enum.
        if p.nth_at(1, TokenKind::Colon) && p.nth_joined(1) {
            let start = p.current_span().start;
            field_decl(p);
            decl.report_other_kind(p, start);
        } else {
            variant_decl(p);
        }
    });
    p.finish_node();
}

/// A `:struct` or `:enum` being parsed, for members written for the other kind.
struct TypeDecl {
    /// Whether it is an enum.
    is_enum: bool,
    /// The span of its keyword, as in `:struct`.
    keyword: Span,
    /// Its name, if it has one.
    name: Option<String>,
    /// Whether a member written for the other kind was reported: once is enough.
    reported: bool,
}

impl TypeDecl {
    /// At the `:` of `:struct` or `:enum`.
    fn new(p: &Parser<'_>) -> Self {
        let keyword = Span::new(p.current_span().start, p.nth_span(1).end);
        let name = p
            .nth_at(2, TokenKind::Ident)
            .then(|| p.nth_text(2).to_owned());
        Self {
            is_enum: p.nth_text(1) == "enum",
            keyword,
            name,
            reported: false,
        }
    }

    /// Reports the member that starts at `start` and ends at the last token consumed,
    /// written for the other kind of declaration, unless one was reported already.
    fn report_other_kind(&mut self, p: &mut Parser<'_>, start: u32) {
        if std::mem::replace(&mut self.reported, true) {
            return;
        }
        let member = Span::new(start, p.previous_end());
        let text = p.text(member);
        let subject = self
            .name
            .as_ref()
            .map_or_else(|| "it".to_owned(), |name| format!("`{name}`"));
        let name = self.name.as_deref().unwrap_or("Name");
        let diagnostic = if self.is_enum {
            Diagnostic::error(
                codes::MEMBER_OF_OTHER_KIND,
                format!("`{text}` looks like a struct field, but {subject} is an enum"),
                member,
            )
            .with_label("variants of an enum are written `name`, or `name field:Type ...`")
            .with_secondary(self.keyword, "declared as an enum here")
            .with_help(format!(
                "if a value has all of these fields at once, declare it with `:struct {name}`; \
                 if it is one of several kinds, give each variant a name, followed by its fields, \
                 as in `point x:f64 y:f64`"
            ))
        } else {
            Diagnostic::error(
                codes::MEMBER_OF_OTHER_KIND,
                format!("`{text}` looks like an enum variant, but {subject} is a struct"),
                member,
            )
            .with_label("fields of a struct are written `name:Type`, one per line")
            .with_secondary(self.keyword, "declared as a struct here")
            .with_help(format!(
                "if a value is one of several kinds, declare it with `:enum {name}`"
            ))
        };
        p.report_always(diagnostic);
    }
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
