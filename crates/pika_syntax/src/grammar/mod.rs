//! The grammar of Pika (section 16 of the specification), as a recursive descent parser.
//!
//! Every function parses one construct starting at the current token. Functions never fail:
//! they report diagnostics and recover, always consuming at least one token when they report an
//! error, so that the parser makes progress.

mod decls;
mod exprs;
mod patterns;
mod stmts;
mod types;

use pika_diagnostics::Diagnostic;

use crate::codes;
use crate::parser::Parser;
use crate::syntax_kind::SyntaxKind;
use crate::token::TokenKind;

pub(crate) use exprs::{at_atom_start, atom};

pub(crate) fn source_file(p: &mut Parser<'_>) {
    p.start_root();
    statement_list(p, TokenKind::Eof, statement);
    p.flush_all();
    p.finish_node();
}

/// Parses items separated by line breaks or `;` until `end` (`}` or the end of the file).
fn statement_list(p: &mut Parser<'_>, end: TokenKind, mut item: impl FnMut(&mut Parser<'_>)) {
    loop {
        while p.at(TokenKind::Newline) || p.at(TokenKind::Semi) {
            p.bump();
        }
        if p.at(TokenKind::Eof) || p.at(end) {
            break;
        }
        if matches!(
            p.current(),
            TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket
        ) {
            let found = p.current().describe();
            let span = p.current_span();
            p.report(Diagnostic::error(
                codes::UNMATCHED_CLOSING,
                format!("unmatched closing {found}"),
                span,
            ));
            p.bump_error();
            continue;
        }
        item(p);
        statement_end(p);
    }
}

/// Expects the end of a statement: a line break, `;`, the closing delimiter, or the end of the
/// file. Anything else is reported and skipped up to the end of the line.
fn statement_end(p: &mut Parser<'_>) {
    match p.current() {
        TokenKind::Newline | TokenKind::Semi => p.bump(),
        // Unmatched closing delimiters are reported by `statement_list`.
        TokenKind::Eof | TokenKind::RBrace | TokenKind::RParen | TokenKind::RBracket => {}
        found => {
            let span = p.current_span();
            p.report(
                Diagnostic::error(
                    codes::EXPECTED_STATEMENT_END,
                    format!(
                        "expected the end of the statement, found {}",
                        found.describe()
                    ),
                    span,
                )
                .with_help("put each statement on its own line, or separate statements with `;`"),
            );
            skip_to_statement_end(p);
        }
    }
}

/// Skips tokens, keeping brackets balanced, until a line break, `;` or unmatched `}`.
fn skip_to_statement_end(p: &mut Parser<'_>) {
    p.start(SyntaxKind::Error);
    let mut depth = 0usize;
    loop {
        match p.current() {
            TokenKind::Eof => break,
            TokenKind::Newline | TokenKind::Semi if depth == 0 => break,
            TokenKind::LParen | TokenKind::LBracket | TokenKind::LBrace => depth += 1,
            TokenKind::RParen | TokenKind::RBracket | TokenKind::RBrace => {
                if depth == 0 {
                    break;
                }
                depth -= 1;
            }
            _ => {}
        }
        p.bump();
    }
    p.finish_node();
}

fn statement(p: &mut Parser<'_>) {
    if p.at(TokenKind::LBrace) {
        block(p);
    } else {
        command(p);
    }
}

/// `{ statements }`
pub(crate) fn block(p: &mut Parser<'_>) {
    p.start(SyntaxKind::Block);
    braced(p, statement);
    p.finish_node();
}

/// `{ items }` where items are separated by line breaks or `;`. Used for blocks and for the
/// bodies of declarations.
fn braced(p: &mut Parser<'_>, item: impl FnMut(&mut Parser<'_>)) {
    let opener = p.current_span();
    if !p.expect(TokenKind::LBrace) {
        return;
    }
    p.with_newlines(true, |p| {
        statement_list(p, TokenKind::RBrace, item);
        p.expect_closing(TokenKind::RBrace, opener);
    });
}

/// Returns true at a token that ends a command's arguments.
fn at_arguments_end(p: &Parser<'_>) -> bool {
    matches!(
        p.current(),
        TokenKind::Newline
            | TokenKind::Semi
            | TokenKind::RBrace
            | TokenKind::RBracket
            | TokenKind::RParen
            | TokenKind::Eof
    )
}

/// Returns true at `name=`, the start of a named argument. Keywords such as `in` and `as`
/// are valid argument names.
fn at_named_arg(p: &Parser<'_>) -> bool {
    matches!(
        p.current(),
        TokenKind::Ident | TokenKind::KwIn | TokenKind::KwAs
    ) && p.nth_at(1, TokenKind::Eq)
}

/// Reports a named argument written with spaces around `=`. Call at the argument name.
fn check_named_arg_spacing(p: &mut Parser<'_>) {
    if p.nth_joined(1) && p.nth_joined(2) {
        return;
    }
    let span = p.current_span().to(p.nth_span(2));
    let name = p.current_text();
    p.report_always(
        Diagnostic::error(
            codes::NAMED_ARG_SPACING,
            "named arguments must be written without spaces around `=`",
            span,
        )
        .with_help(format!("write `{name}=...`")),
    );
}

/// A command: a built-in form such as `:if`, or a call.
pub(crate) fn command(p: &mut Parser<'_>) {
    match p.current() {
        TokenKind::Colon => colon_command(p),
        // A value as the head: `[$s->trim]->split " "` calls a method of the value of a
        // command, and `[:adder 1] 5` calls the function value it gives.
        TokenKind::Variable
        | TokenKind::Slash
        | TokenKind::KwSelfType
        | TokenKind::KwSome
        | TokenKind::LBracket
        | TokenKind::LParen
        | TokenKind::Int
        | TokenKind::Float
        | TokenKind::Duration
        | TokenKind::Char
        | TokenKind::RawString
        | TokenKind::StringStart
        | TokenKind::KwTrue
        | TokenKind::KwFalse
        | TokenKind::LBrace => {
            // A literal is only a head with a method, as in `["a,b"->split ","]`; a block
            // starts a statement before this.
            call(p);
        }
        TokenKind::Minus
            if p.nth_joined(1) && matches!(p.nth(1), TokenKind::Int | TokenKind::Float) =>
        {
            call(p);
        }
        TokenKind::Ident if at_type_member_or_literal(p) => call(p),
        TokenKind::Ident => not_a_command(p),
        _ => {
            p.error_expected("a command");
            if !at_arguments_end(p) {
                p.bump_error();
            }
        }
    }
}

/// Returns true at `Name->...`, `Name<...>` or `Name{...}`: a type name used as the head of a
/// static member access or a struct literal.
fn at_type_member_or_literal(p: &Parser<'_>) -> bool {
    (p.nth_at(1, TokenKind::Arrow) && p.nth_joined(1))
        || (p.nth_joined(1) && matches!(p.nth(1), TokenKind::Lt | TokenKind::LBrace))
}

/// A statement that starts with a bare name, such as `put "hi"` instead of `:put "hi"`.
fn not_a_command(p: &mut Parser<'_>) {
    let name = p.current_text();
    let span = p.current_span();
    p.report(
        Diagnostic::error(
            codes::NOT_A_COMMAND,
            format!("`{name}` is not a command"),
            span,
        )
        .with_help(format!("commands start with `:`, as in `:{name}`")),
    );
    p.start(SyntaxKind::Call);
    p.bump_error();
    arg_list(p);
    p.finish_node();
}

fn colon_command(p: &mut Parser<'_>) {
    let name_follows = p.nth_at(1, TokenKind::Ident);
    if !name_follows || !p.nth_joined(1) {
        let span = p.current_span();
        p.report(
            Diagnostic::error(
                codes::COMMAND_NAME_SPACING,
                "expected a command name directly after `:`",
                span,
            )
            .with_help("write commands as `:name`, for example `:put`"),
        );
        if !name_follows {
            p.bump_error();
            return;
        }
    }
    match p.nth_text(1) {
        "local" => stmts::var_decl(p, SyntaxKind::LocalDecl),
        "const" => stmts::var_decl(p, SyntaxKind::ConstDecl),
        "global" => stmts::var_decl(p, SyntaxKind::GlobalDecl),
        "set" => stmts::set_stmt(p),
        "if" => stmts::if_stmt(p),
        "while" => stmts::while_stmt(p),
        "do" => stmts::do_while_stmt(p),
        "for" => stmts::for_stmt(p),
        "foreach" => stmts::foreach_stmt(p),
        "match" => stmts::match_stmt(p),
        "onerror" => stmts::onerror_stmt(p),
        "unsafe" => stmts::unsafe_block(p),
        "fn" => decls::fn_decl(p),
        "struct" => decls::struct_decl(p),
        "enum" => decls::enum_decl(p),
        "trait" => decls::trait_decl(p),
        "impl" => decls::impl_decl(p),
        "use" => decls::use_decl(p),
        "extern" => decls::extern_decl(p),
        "test" => decls::test_decl(p),
        _ => call(p),
    }
}

/// Starts a built-in form: opens `kind` and consumes `:name`.
fn start_form(p: &mut Parser<'_>, kind: SyntaxKind) {
    p.start(kind);
    p.bump(); // `:`
    p.bump(); // name
}

/// A call: a head (`:name`, `/path`, `$value`, `Type->member`, or a bracketed command or
/// parenthesized expression) followed by arguments.
fn call(p: &mut Parser<'_>) {
    p.start(SyntaxKind::Call);
    if p.at(TokenKind::Colon) {
        p.start(SyntaxKind::CommandName);
        p.bump();
        p.bump();
        if p.at(TokenKind::Lt) && p.at_joined() {
            types::generic_args(p);
        }
        p.finish_node();
    } else if p.at(TokenKind::KwSome) {
        p.start(SyntaxKind::SomeHead);
        p.bump();
        p.finish_node();
    } else {
        atom(p);
    }
    // `?` joined to the head marks a call that can raise an error.
    if p.at(TokenKind::Question) && p.at_joined() {
        p.bump();
    }
    arg_list(p);
    p.finish_node();
}

/// Positional and named arguments of a call.
fn arg_list(p: &mut Parser<'_>) {
    p.start(SyntaxKind::ArgList);
    loop {
        if at_arguments_end(p) {
            break;
        }
        if at_named_arg(p) {
            named_arg(p);
        } else if at_atom_start(p) {
            atom(p);
        } else if exprs::binary_op_at(p).is_some() || p.at(TokenKind::Bang) {
            operator_in_arguments(p);
        } else {
            break;
        }
    }
    p.finish_node();
}

fn operator_in_arguments(p: &mut Parser<'_>) {
    let op = p.current_text();
    let span = p.current_span();
    p.report(
        Diagnostic::error(
            codes::OPERATOR_IN_ARGUMENTS,
            format!("operator `{op}` in a command argument must be inside parentheses"),
            span,
        )
        .with_help(format!(
            "write the expression in parentheses, for example `($a {op} $b)`"
        )),
    );
    p.bump_error();
}

/// `name=value` in a call.
fn named_arg(p: &mut Parser<'_>) {
    p.start(SyntaxKind::NamedArg);
    check_named_arg_spacing(p);
    name_ref(p);
    p.bump(); // `=`
    if !atom(p) {
        p.error_expected("an argument value");
    }
    p.finish_node();
}

/// The shape of a named argument of a built-in form.
#[derive(Clone, Copy, PartialEq, Eq)]
enum ArgShape {
    /// `do={ ... }`
    Block,
    /// `else={ ... }` or `else=:if ...`
    BlockOrIf,
    /// `while=(condition)`
    Paren,
    /// `from=atom`
    Atom,
    /// `as=name`
    Name,
    /// `impl=Trait1,Trait2`
    ImplList,
}

#[derive(Clone, Copy)]
struct ArgSpec {
    name: &'static str,
    shape: ArgShape,
    required: bool,
}

const fn arg(name: &'static str, shape: ArgShape, required: bool) -> ArgSpec {
    ArgSpec {
        name,
        shape,
        required,
    }
}

/// Parses the named arguments of the built-in form `:command`, checking them against `specs`.
fn form_args(p: &mut Parser<'_>, command: &str, specs: &[ArgSpec]) {
    let mut seen = Vec::new();
    loop {
        if at_named_arg(p) {
            let name = p.current_text();
            let spec = specs.iter().find(|s| s.name == name);
            if spec.is_none() {
                let span = p.current_span();
                let expected: Vec<String> =
                    specs.iter().map(|s| format!("`{}=`", s.name)).collect();
                p.report(
                    Diagnostic::error(
                        codes::UNKNOWN_ARGUMENT,
                        format!("`:{command}` has no argument named `{name}`"),
                        span,
                    )
                    .with_help(format!("`:{command}` accepts {}", expected.join(", "))),
                );
            }
            seen.push(name);
            p.start(SyntaxKind::NamedArg);
            check_named_arg_spacing(p);
            name_ref(p);
            p.bump(); // `=`
            form_arg_value(p, spec.map_or(ArgShape::Atom, |s| s.shape));
            p.finish_node();
        } else if at_atom_start(p) && !p.at(TokenKind::LBrace) {
            // A `{` after the arguments is the body of `:struct`, `:match` and similar forms.
            // Where no body is expected, the caller reports it as the end of the statement.
            let span = p.current_span();
            p.report(Diagnostic::error(
                codes::UNKNOWN_ARGUMENT,
                format!("unexpected argument to `:{command}`"),
                span,
            ));
            p.start(SyntaxKind::Error);
            atom(p);
            p.finish_node();
        } else {
            break;
        }
    }
    let missing: Vec<String> = specs
        .iter()
        .filter(|s| s.required && !seen.contains(&s.name))
        .map(|spec| {
            let example = match spec.shape {
                ArgShape::Block | ArgShape::BlockOrIf => "{...}",
                ArgShape::Paren => "(...)",
                ArgShape::Atom | ArgShape::Name | ArgShape::ImplList => "...",
            };
            format!("`{}={example}`", spec.name)
        })
        .collect();
    if !missing.is_empty() {
        let span = p.current_span();
        let mut diagnostic = Diagnostic::error(
            codes::MISSING_ARGUMENT,
            format!("`:{command}` requires {}", missing.join(" and ")),
            span,
        );
        let block_missing = specs
            .iter()
            .any(|s| s.required && s.shape == ArgShape::Block && !seen.contains(&s.name));
        if block_missing && p.at(TokenKind::LBrace) {
            diagnostic =
                diagnostic.with_help("blocks are passed as named arguments, as in `do={...}`");
        }
        p.report(diagnostic);
    }
}

fn form_arg_value(p: &mut Parser<'_>, shape: ArgShape) {
    match shape {
        ArgShape::Block => block(p),
        ArgShape::BlockOrIf => {
            if p.at(TokenKind::Colon) && p.nth_text(1) == "if" {
                colon_command(p);
            } else {
                block(p);
            }
        }
        ArgShape::Paren => exprs::paren_condition(p),
        ArgShape::Atom => {
            if !atom(p) {
                p.error_expected("an argument value");
            }
        }
        ArgShape::Name => name(p),
        ArgShape::ImplList => types::impl_list(p),
    }
}

/// A reference to a name: the current token wrapped in a [`SyntaxKind::NameRef`] node.
fn name_ref(p: &mut Parser<'_>) {
    p.start(SyntaxKind::NameRef);
    p.bump();
    p.finish_node();
}

/// A declared name: an identifier wrapped in a [`SyntaxKind::Name`] node.
fn name(p: &mut Parser<'_>) {
    if p.at(TokenKind::Ident) {
        p.start(SyntaxKind::Name);
        p.bump();
        p.finish_node();
    } else if p.at(TokenKind::Variable) {
        let text = p.current_text();
        let span = p.current_span();
        p.report(
            Diagnostic::error(
                codes::DOLLAR_IN_DECLARATION,
                format!("expected a name, found the variable read `{text}`"),
                span,
            )
            .with_help(format!(
                "names are written without `$` where they are declared or assigned: `{}`",
                &text[1..]
            )),
        );
        p.start(SyntaxKind::Name);
        p.bump();
        p.finish_node();
    } else {
        p.error_expected("a name");
    }
}

/// `:Type` directly after a name, as in `x:i64`.
fn type_annotation(p: &mut Parser<'_>) {
    p.start(SyntaxKind::TypeAnnotation);
    if !p.at_joined() || !p.nth_joined(1) {
        let span = p.current_span();
        p.report(
            Diagnostic::error(
                codes::TYPE_ANNOTATION_SPACING,
                "type annotations must be written without spaces around `:`",
                span,
            )
            .with_help("write `name:Type`, for example `x:i64`"),
        );
    }
    p.bump(); // `:`
    types::type_(p);
    p.finish_node();
}

/// `=value` directly after a type, as in `greeting:String="Hello"`.
fn default_value(p: &mut Parser<'_>) {
    p.start(SyntaxKind::DefaultValue);
    p.bump(); // `=`
    if !atom(p) {
        p.error_expected("a default value");
    }
    p.finish_node();
}
