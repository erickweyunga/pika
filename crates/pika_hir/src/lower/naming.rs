//! The naming conventions (spec section 3.5), checked as warnings at the declarations of the
//! package being compiled: `PascalCase` for types, traits and type parameters, and
//! `snake_case` for every other name.

use std::collections::HashSet;

use pika_diagnostics::{Diagnostic, Span};

use crate::{Body, FnKind, Generics, LocalKind, Module, ModuleId, codes};

/// How a kind of name is written.
#[derive(Clone, Copy)]
enum Case {
    Pascal,
    Snake,
}

impl Case {
    fn name(self) -> &'static str {
        match self {
            Self::Pascal => "PascalCase",
            Self::Snake => "snake_case",
        }
    }

    /// `name` written in this case, if it is not already. A leading `_`, which makes an item
    /// private, is not part of the name.
    fn fix(self, name: &str) -> Option<String> {
        let words = name.trim_start_matches('_');
        let prefix = &name[..name.len() - words.len()];
        if words.is_empty() {
            return None;
        }
        let fixed = match self {
            Self::Pascal if is_pascal(words) => return None,
            Self::Snake if !words.chars().any(|c| c.is_ascii_uppercase()) => return None,
            Self::Pascal => to_pascal(words),
            Self::Snake => to_snake(words),
        };
        Some(format!("{prefix}{fixed}"))
    }
}

fn is_pascal(name: &str) -> bool {
    name.starts_with(|c: char| c.is_ascii_uppercase()) && !name.contains('_')
}

/// `name` in `PascalCase`: each word starts with a capital, and a word written all in
/// capitals keeps only its first one.
fn to_pascal(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for word in name.split('_').filter(|word| !word.is_empty()) {
        let shouting = !word.chars().any(|c| c.is_ascii_lowercase());
        let mut chars = word.chars();
        if let Some(first) = chars.next() {
            out.push(first.to_ascii_uppercase());
        }
        if shouting {
            out.extend(chars.map(|c| c.to_ascii_lowercase()));
        } else {
            out.extend(chars);
        }
    }
    out
}

/// `name` in `snake_case`: a new word starts at each capital that follows a small letter or
/// digit, and at the last capital of a run of them that a small letter follows, so that
/// `HTTPServer` is `http_server`.
fn to_snake(name: &str) -> String {
    let chars: Vec<char> = name.chars().collect();
    let mut out = String::with_capacity(name.len() + 4);
    for (index, &c) in chars.iter().enumerate() {
        if c.is_ascii_uppercase() && index > 0 {
            let before = chars[index - 1];
            let after = chars.get(index + 1).copied();
            let starts_word = before.is_ascii_lowercase()
                || before.is_ascii_digit()
                || (before.is_ascii_uppercase() && after.is_some_and(|a| a.is_ascii_lowercase()));
            if starts_word && !out.ends_with('_') {
                out.push('_');
            }
        }
        out.push(c.to_ascii_lowercase());
    }
    out
}

/// Collects the warnings, one per declaration.
#[derive(Default)]
struct Check {
    seen: HashSet<Span>,
    diagnostics: Vec<Diagnostic>,
}

impl Check {
    /// Checks the name `name`, declared at `span`, of a `what` written in `case`.
    fn name(&mut self, what: &str, name: &str, span: Span, case: Case) {
        let Some(fixed) = case.fix(name) else {
            return;
        };
        // A declaration can be shared, as a trait's function by the types that implement it.
        if !self.seen.insert(span) {
            return;
        }
        self.diagnostics.push(
            Diagnostic::warning(
                codes::NAMING_CONVENTION,
                format!("{what} `{name}` should be written in {}", case.name()),
                span,
            )
            .with_label(format!("should be `{fixed}`")),
        );
    }

    fn generics(&mut self, generics: &Generics) {
        for param in &generics.params {
            self.name(
                "type parameter",
                &param.name.value,
                param.name.span,
                Case::Pascal,
            );
        }
    }

    fn locals(&mut self, body: &Body) {
        for (_, local) in body.locals.iter() {
            let what = match local.kind {
                // The variable of the enclosing function, checked where it is declared.
                LocalKind::Captured => continue,
                LocalKind::Param(_) => "parameter",
                _ => "variable",
            };
            if local.name.value != "self" {
                self.name(what, &local.name.value, local.name.span, Case::Snake);
            }
        }
    }
}

/// Warnings about the names declared by the package that `module` compiles that do not follow
/// the naming conventions.
pub(super) fn check(module: &Module) -> Vec<Diagnostic> {
    let checked = |id: ModuleId| module.in_root_package(id);
    let mut check = Check::default();
    for (_, def) in module.types.iter().filter(|(_, def)| checked(def.module)) {
        check.name("type", &def.name.value, def.name.span, Case::Pascal);
        check.generics(&def.generics);
        for field in &def.fields {
            check.name("field", &field.name.value, field.name.span, Case::Snake);
        }
        for variant in &def.variants {
            check.name(
                "variant",
                &variant.name.value,
                variant.name.span,
                Case::Snake,
            );
            for field in &variant.fields {
                check.name("field", &field.name.value, field.name.span, Case::Snake);
            }
        }
    }
    for (_, def) in module.traits.iter().filter(|(_, def)| checked(def.module)) {
        check.name("trait", &def.name.value, def.name.span, Case::Pascal);
    }
    for (_, function) in module.functions.iter() {
        if !checked(function.module) {
            continue;
        }
        if function.kind == FnKind::Declared {
            let what = if function.owner.is_some() {
                "method"
            } else {
                "function"
            };
            check.name(what, &function.name.value, function.name.span, Case::Snake);
            check.generics(&function.generics);
        }
        check.locals(&function.body);
    }
    for (_, item) in module
        .consts
        .iter()
        .filter(|(_, item)| checked(item.module))
    {
        check.name("constant", &item.name.value, item.name.span, Case::Snake);
    }
    for (_, item) in module
        .globals
        .iter()
        .filter(|(_, item)| checked(item.module))
    {
        check.name("global", &item.name.value, item.name.span, Case::Snake);
    }
    check.diagnostics
}

#[cfg(test)]
mod tests {
    use super::Case;

    #[test]
    fn names_are_fixed() {
        let pascal = |name| Case::Pascal.fix(name);
        let snake = |name| Case::Snake.fix(name);
        assert_eq!(pascal("student"), Some("Student".to_owned()));
        assert_eq!(pascal("http_server"), Some("HttpServer".to_owned()));
        assert_eq!(pascal("HTTP_SERVER"), Some("HttpServer".to_owned()));
        assert_eq!(pascal("_cache"), Some("_Cache".to_owned()));
        assert_eq!(pascal("HttpServer"), None);
        assert_eq!(pascal("T"), None);
        assert_eq!(pascal("_Cache"), None);
        assert_eq!(snake("bestScore"), Some("best_score".to_owned()));
        assert_eq!(snake("HTTPServer"), Some("http_server".to_owned()));
        assert_eq!(snake("LIMIT"), Some("limit".to_owned()));
        assert_eq!(snake("MAX_SIZE"), Some("max_size".to_owned()));
        assert_eq!(snake("_Helper"), Some("_helper".to_owned()));
        assert_eq!(snake("vec2d"), None);
        assert_eq!(snake("_helper"), None);
        assert_eq!(snake("_"), None);
    }
}
