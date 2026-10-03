//! Advice for a type that lacks a built-in trait an operation needs: which trait it is, and
//! where and how to add it.

use pika_diagnostics::{Diagnostic, Span};
use pika_hir::{AdtKind, Bound, BuiltinTrait, TypeDef, TypeId};
use pika_ty::Ty;

use super::{Cx, Requirement, adt_of, bound_hint, satisfies};

/// What a diagnostic about an unmet requirement says about the trait the type lacks.
#[derive(Default)]
pub(super) struct Advice {
    /// The type, as shown.
    subject: String,
    /// The traits the type lacks, as `` `Hash` and `Eq` ``.
    pub(super) missing: Option<String>,
    /// What the type cannot do, for the label of the diagnostic.
    pub(super) label: Option<String>,
    /// The declaration to change, or the part of it that is in the way, with its label.
    pub(super) declared: Option<(Span, String)>,
    /// How to fix it.
    pub(super) help: Option<String>,
}

impl Advice {
    /// Adds the advice to a diagnostic.
    pub(super) fn apply(self, mut diagnostic: Diagnostic) -> Diagnostic {
        if let Some(missing) = &self.missing {
            let subject = &self.subject;
            diagnostic.message = if diagnostic.message.ends_with(subject.as_str()) {
                format!("{}, which does not implement {missing}", diagnostic.message)
            } else {
                format!(
                    "{}, as {subject} does not implement {missing}",
                    diagnostic.message
                )
            };
        }
        if let Some(label) = self.label {
            diagnostic = diagnostic.with_label(label);
        }
        if let Some((span, label)) = self.declared {
            diagnostic = diagnostic.with_secondary(span, label);
        }
        if let Some(help) = self.help {
            diagnostic = diagnostic.with_help(help);
        }
        diagnostic
    }
}

/// The built-in trait a requirement is, if it is one.
fn builtin_trait(requirement: Requirement) -> Option<BuiltinTrait> {
    Some(match requirement {
        Requirement::Eq => BuiltinTrait::Eq,
        Requirement::Ord => BuiltinTrait::Ord,
        Requirement::Display => BuiltinTrait::Display,
        Requirement::Copy => BuiltinTrait::Copy,
        Requirement::Clone => BuiltinTrait::Clone,
        Requirement::Hash => BuiltinTrait::Hash,
        Requirement::Default => BuiltinTrait::Default,
        _ => return None,
    })
}

/// What a value lacking a built-in trait cannot do.
fn inability(which: BuiltinTrait) -> &'static str {
    match which {
        BuiltinTrait::Eq => "cannot be compared",
        BuiltinTrait::Ord => "has no order",
        BuiltinTrait::Display => "cannot be displayed",
        BuiltinTrait::Copy => "cannot be copied",
        BuiltinTrait::Clone => "cannot be cloned",
        BuiltinTrait::Hash => "cannot be hashed",
        BuiltinTrait::Default => "has no default value",
    }
}

/// Advice for `ty`, which does not meet `requirement`, a built-in trait (for `Key`, `Hash`
/// and `Eq`). `ty` is shown as `shown`.
pub(super) fn advise(cx: Cx<'_>, ty: Ty, shown: &str, requirement: Requirement) -> Advice {
    // The built-in traits the type lacks.
    let lacking: Vec<BuiltinTrait> = match requirement {
        Requirement::Key => [Requirement::Hash, Requirement::Eq]
            .into_iter()
            .filter(|&r| !satisfies(cx, ty, r))
            .filter_map(builtin_trait)
            .collect(),
        other => builtin_trait(other).into_iter().collect(),
    };
    let Some(&first) = lacking.first() else {
        return Advice::default();
    };
    let names: Vec<String> = lacking.iter().map(|t| format!("`{}`", t.name())).collect();
    let mut advice = Advice {
        subject: shown.to_owned(),
        missing: Some(names.join(" or ")),
        label: Some(format!("{shown} {}", inability(first))),
        ..Advice::default()
    };
    // In a collection, an option or a box, the part that lacks the trait.
    let culprit = culprit(cx, ty, &lacking);
    let culprit_shown = format!("`{culprit}`");
    if let Ty::Param(param) = culprit {
        advice.help = Some(bound_hint(cx.module, &param.name, requirement_of(&lacking)));
    } else if let Some(id) = adt_of(cx.module, culprit) {
        advise_adt(cx, id, culprit, &lacking, &mut advice);
    } else {
        advice.help = never(culprit, first).map(|reason| {
            if culprit == ty {
                reason.to_owned()
            } else {
                format!("{shown} contains {culprit_shown}, and {reason}")
            }
        });
    }
    advice
}

/// The requirement of a type parameter that lacks `lacking`, for its bound.
fn requirement_of(lacking: &[BuiltinTrait]) -> Requirement {
    match lacking {
        [BuiltinTrait::Hash, BuiltinTrait::Eq] => Requirement::Key,
        [which, ..] => super::bound_requirement(Bound::Builtin(*which)),
        [] => unreachable!("a type that lacks no trait has no advice"),
    }
}

/// The part of `ty` that keeps it from having the traits `lacking`: `ty` itself, or the
/// first element, value or key type of a collection, option or box that lacks one.
fn culprit(cx: Cx<'_>, ty: Ty, lacking: &[BuiltinTrait]) -> Ty {
    let container = matches!(
        ty,
        Ty::Option(_) | Ty::Box(_) | Ty::List(_) | Ty::Set(_) | Ty::Map(_)
    );
    if !container {
        return ty;
    }
    ty.components()
        .into_iter()
        .find(|&part| {
            lacking
                .iter()
                .any(|&which| !satisfies(cx, part, super::bound_requirement(Bound::Builtin(which))))
        })
        .map_or(ty, |part| culprit(cx, part, lacking))
}

/// Why a built-in type can never have the trait `which`, if it cannot.
fn never(ty: Ty, which: BuiltinTrait) -> Option<&'static str> {
    Some(match (ty, which) {
        (Ty::Fn(_), _) => "function values can only be called and cloned",
        (Ty::Float(_), BuiltinTrait::Hash) => {
            "floats cannot be hashed, so they cannot be map keys or set elements"
        }
        (Ty::List(_) | Ty::Set(_) | Ty::Map(_), BuiltinTrait::Hash) => {
            "collections cannot be hashed, so they cannot be map keys or set elements"
        }
        (Ty::Set(_) | Ty::Map(_), BuiltinTrait::Ord) => {
            "sets and maps have no order: only lists, numbers, characters, strings and durations compare with `<`"
        }
        (Ty::Nothing, BuiltinTrait::Display) => {
            "`nothing` has no text: the expression does not produce a value"
        }
        _ => return None,
    })
}

/// Advice for the struct or enum `id`, of type `ty`, which lacks the traits `lacking`.
fn advise_adt(cx: Cx<'_>, id: TypeId, ty: Ty, lacking: &[BuiltinTrait], advice: &mut Advice) {
    let module = cx.module;
    let def = &module.types[id];
    let name = &def.name.value;
    // A trait the type lists that holds only for type arguments whose fields have it.
    let listed: Vec<BuiltinTrait> = lacking
        .iter()
        .copied()
        .filter(|&which| derive_span(def, which).is_some())
        .collect();
    if let Some(&which) = listed.first() {
        let requirement = super::bound_requirement(Bound::Builtin(which));
        let args = ty.components();
        let field = def
            .fields
            .iter()
            .chain(def.variants.iter().flat_map(|v| &v.fields))
            .find(|f| !satisfies(cx, f.ty.value.subst(&args), requirement));
        if let Some(field) = field {
            let field_ty = field.ty.value.subst(&args);
            advice.declared = Some((field.ty.span, format!("not `{}` for `{ty}`", which.name())));
            let mut help = format!(
                "`{name}` implements `{}` only when its fields do, and the field `{}` of `{ty}` has type `{field_ty}`, which does not",
                which.name(),
                field.name.value,
            );
            // How to give the field's type the trait, when there is a way.
            if let Some(fix) = advise(cx, field_ty, &format!("`{field_ty}`"), requirement).help {
                help = format!("{help}; {fix}");
            }
            advice.help = Some(help);
        }
        return;
    }
    if !module.in_root_package(def.module) {
        advice.help = Some(format!(
            "`{name}` is declared in `{}`, outside this package, which would have to add {}",
            module.modules[def.module].display_path(),
            advice.missing.as_deref().unwrap_or_default(),
        ));
        return;
    }
    let declared = if advice.subject == format!("`{ty}`") {
        "declared here".to_owned()
    } else {
        format!("`{name}` is declared here")
    };
    advice.declared = Some((def.name.span, declared));
    if lacking == [BuiltinTrait::Default] && def.kind == AdtKind::Enum {
        advice.help = Some(format!(
            "an enum has no default variant to derive `Default` from; define it in `{name}`: `:fn default -> Self do={{ :return {name}->... }}`"
        ));
        return;
    }
    let added: Vec<&str> = lacking.iter().map(|t| t.name()).collect();
    let mut traits = written_traits(cx, def);
    let had_list = !traits.is_empty();
    traits.extend(added.iter().map(|&t| t.to_owned()));
    let header = format!(
        ":{} {name}{} impl={} {{",
        if def.kind == AdtKind::Enum {
            "enum"
        } else {
            "struct"
        },
        written_generics(cx, def),
        traits.join(",")
    );
    advice.help = Some(if had_list {
        format!(
            "add {} to the `impl=` list of `{name}`: `{header}`",
            quoted(&added)
        )
    } else {
        format!("add `impl={}` to `{name}`: `{header}`", added.join(","))
    });
}

/// `` `A` and `B` `` for the names `A` and `B`.
fn quoted(names: &[&str]) -> String {
    names
        .iter()
        .map(|name| format!("`{name}`"))
        .collect::<Vec<_>>()
        .join(" and ")
}

/// Where a type lists the built-in trait `which`, if it does.
fn derive_span(def: &TypeDef, which: BuiltinTrait) -> Option<Span> {
    let derives = &def.derives;
    match which {
        BuiltinTrait::Copy => derives.copy,
        BuiltinTrait::Clone => derives.clone,
        BuiltinTrait::Eq => derives.eq,
        BuiltinTrait::Ord => derives.ord,
        BuiltinTrait::Hash => derives.hash,
        BuiltinTrait::Display => derives.display,
        BuiltinTrait::Default => derives.default,
    }
}

/// The traits a type lists in `impl=`, in the order they are written.
fn written_traits(cx: Cx<'_>, def: &TypeDef) -> Vec<String> {
    let all = [
        BuiltinTrait::Copy,
        BuiltinTrait::Clone,
        BuiltinTrait::Eq,
        BuiltinTrait::Ord,
        BuiltinTrait::Hash,
        BuiltinTrait::Display,
        BuiltinTrait::Default,
    ];
    let mut listed: Vec<(Span, String)> = all
        .into_iter()
        .filter_map(|which| derive_span(def, which).map(|span| (span, which.name().to_owned())))
        .chain(
            def.traits
                .iter()
                .map(|t| (t.span, cx.module.traits[t.value].name.value.clone())),
        )
        .collect();
    listed.sort_by_key(|(span, _)| span.start);
    listed.into_iter().map(|(_, name)| name).collect()
}

/// The type parameters of a type as written, as in `<T: Display, U>`, or nothing.
fn written_generics(cx: Cx<'_>, def: &TypeDef) -> String {
    if def.generics.params.is_empty() {
        return String::new();
    }
    let params: Vec<String> = def
        .generics
        .params
        .iter()
        .map(|param| {
            let bounds: Vec<&str> = param
                .bounds
                .iter()
                .map(|bound| match bound.value {
                    Bound::Builtin(which) => which.name(),
                    Bound::Trait(id) => cx.module.traits[id].name.value.as_str(),
                })
                .collect();
            if bounds.is_empty() {
                param.name.value.clone()
            } else {
                format!("{}: {}", param.name.value, bounds.join(" + "))
            }
        })
        .collect();
    format!("<{}>", params.join(", "))
}
