//! Checks of user-defined traits and of the types that implement them (spec section 10.3).

use std::fmt::Write;

use pika_diagnostics::Diagnostic;
use pika_hir::{
    Bound, BuiltinTrait, Convention, Derives, FnId, Function, ImplId, Module, PreludeTrait,
    TraitId, TypeId,
};
use pika_ty::Ty;

use super::{Cx, ModuleChecker, builtin_method, implied_bounds};
use crate::codes;

impl ModuleChecker<'_> {
    /// Reports the functions of an `:impl` of a built-in type that have the name of one of its
    /// built-in methods, which they would hide.
    pub(super) fn check_builtin_impl(&mut self, id: ImplId) {
        let module = self.module;
        let def = &module.impls[id];
        let cx = Cx::new(module, &def.generics.params);
        for (name, function) in &def.functions {
            let builtin = name == "clone"
                || builtin_method(cx, name, def.self_ty.value).is_some()
                || (def.self_ty.value == Ty::Formatter && name == "write");
            if builtin {
                self.report(Diagnostic::error(
                    pika_hir::codes::INVALID_IMPL,
                    format!(
                        "`{}` already has a built-in method named `{name}`",
                        def.self_ty.value
                    ),
                    module.functions[*function].name.span,
                ));
            }
        }
    }

    /// Reports a trait that requires itself through the traits it requires.
    pub(super) fn check_trait(&mut self, id: TraitId) {
        let module = self.module;
        let def = &module.traits[id];
        for supertrait in &def.supertraits {
            let Bound::Trait(other) = supertrait.value else {
                continue;
            };
            if implied_bounds(module, Bound::Trait(other)).contains(&Bound::Trait(id)) {
                self.report(Diagnostic::error(
                    codes::INVALID_IMPL,
                    format!(
                        "the trait `{}` requires itself, through `{}`",
                        def.name.value, module.traits[other].name.value
                    ),
                    supertrait.span,
                ));
            }
        }
    }

    /// Checks that a struct or enum implements the traits in its `impl=`: it implements the
    /// traits they require, defines their required functions, and defines its versions of
    /// their functions with the same signatures.
    pub(super) fn check_impls(&mut self, id: TypeId) {
        let module = self.module;
        let def = &module.types[id];
        for listed in &def.traits {
            let trait_def = &module.traits[listed.value];
            for supertrait in &trait_def.supertraits {
                let met = match supertrait.value {
                    Bound::Trait(t) => def.traits.iter().any(|l| l.value == t),
                    Bound::Builtin(builtin) => derives(&def.derives, builtin),
                };
                if !met {
                    let required = bound_name(module, supertrait.value);
                    let help = format!("add `{required}` to the `impl=` of `{}`", def.name.value);
                    self.report(
                        Diagnostic::error(
                            codes::INVALID_IMPL,
                            format!(
                                "`{}` implements `{}`, which requires `{required}`",
                                def.name.value, trait_def.name.value
                            ),
                            listed.span,
                        )
                        .with_help(help),
                    );
                }
            }
            for (name, function) in &trait_def.functions {
                match def.function(name) {
                    Some(own) => self.check_impl_fn(id, listed.value, *function, own),
                    None if !module.functions[*function].has_body => {
                        let required = &module.functions[*function];
                        let diagnostic = Diagnostic::error(
                            codes::INVALID_IMPL,
                            format!(
                                "`{}` does not define `{name}`, which the trait `{}` requires",
                                def.name.value, trait_def.name.value
                            ),
                            listed.span,
                        )
                        .with_help(format!(
                            "add it to `{}`: `{} do={{ ... }}`",
                            def.name.value,
                            signature(required, &[def.ty])
                        ));
                        self.report(at_declaration(
                            diagnostic,
                            module,
                            listed.value,
                            required,
                            "required here",
                        ));
                    }
                    None => {}
                }
            }
        }
        self.check_inherited_conflicts(id);
        self.check_default_fn(id);
        self.check_fmt_fn(id);
        let drop_trait = module.prelude_trait(PreludeTrait::Drop);
        if let Some(copy) = def.derives.copy
            && def.traits.iter().any(|t| t.value == drop_trait)
        {
            self.report(
                Diagnostic::error(
                    codes::INVALID_IMPL,
                    format!("`{}` cannot be both `Copy` and `Drop`", def.name.value),
                    copy,
                )
                .with_help("a copied value would run `drop` once for each copy"),
            );
        }
    }

    /// Checks the signature of a type's own `default`, used by `[:default]`.
    fn check_default_fn(&mut self, id: TypeId) {
        let module = self.module;
        let def = &module.types[id];
        let (Some(_), Some(function)) = (def.derives.default, def.function("default")) else {
            return;
        };
        let function = &module.functions[function];
        let fits = function.params.is_empty()
            && function.generics.params.len() == function.own_generics
            && function.ret.value == def.ty
            && !function.raises;
        if !fits {
            self.report(
                Diagnostic::error(
                    codes::INVALID_IMPL,
                    format!(
                        "`{}->default` does not match the trait `Default`",
                        def.name.value
                    ),
                    function.name.span,
                )
                .with_help("declare it as `:fn default -> Self`"),
            );
        }
    }

    /// Checks the signature of a type's own `fmt`, which displays its values.
    fn check_fmt_fn(&mut self, id: TypeId) {
        let module = self.module;
        let def = &module.types[id];
        let (Some(_), Some(function)) = (def.derives.display, def.function("fmt")) else {
            return;
        };
        let function = &module.functions[function];
        let fits = matches!(
            function.params.as_slice(),
            [this, out] if this.is_self
                && this.convention == Convention::Read
                && out.convention == Convention::Mut
                && out.ty.value == Ty::Formatter
        ) && function.generics.params.len() == function.own_generics
            && function.ret.value == Ty::Nothing
            && !function.raises;
        if !fits {
            self.report(
                Diagnostic::error(
                    codes::INVALID_IMPL,
                    format!(
                        "`{}->fmt` does not match the trait `Display`",
                        def.name.value
                    ),
                    function.name.span,
                )
                .with_help("declare it as `:fn fmt self mut out:Formatter`"),
            );
        }
    }

    /// Reports functions that a type gets from two of its traits.
    fn check_inherited_conflicts(&mut self, id: TypeId) {
        let module = self.module;
        let def = &module.types[id];
        for (index, first) in def.traits.iter().enumerate() {
            for second in &def.traits[index + 1..] {
                for (name, _) in &module.traits[first.value].functions {
                    if def.function(name).is_none()
                        && module.traits[second.value].function(name).is_some()
                    {
                        self.report(
                            Diagnostic::error(
                                codes::INVALID_IMPL,
                                format!(
                                    "the traits `{}` and `{}` of `{}` both have a function named `{name}`",
                                    module.traits[first.value].name.value,
                                    module.traits[second.value].name.value,
                                    def.name.value
                                ),
                                second.span,
                            )
                            .with_help(format!(
                                "define `{name}` in `{}` itself, matching both traits",
                                def.name.value
                            )),
                        );
                    }
                }
            }
        }
    }

    /// Checks that function `own` of type `ty` matches function `required` of trait `tr`.
    fn check_impl_fn(&mut self, ty: TypeId, tr: TraitId, required: FnId, own: FnId) {
        let module = self.module;
        let def = &module.types[ty];
        let (required_fn, own_fn) = (&module.functions[required], &module.functions[own]);
        // The trait's function, as the type's: `Self` is the type, and the trait function's
        // own type parameters are the type function's.
        let own_params: Vec<Ty> = own_fn.generics.params[own_fn.own_generics..]
            .iter()
            .map(|p| p.ty)
            .collect();
        let subst: Vec<Ty> = std::iter::once(def.ty).chain(own_params).collect();
        let Some(difference) = signature_difference(required_fn, own_fn, &subst) else {
            return;
        };
        let diagnostic = Diagnostic::error(
            codes::INVALID_IMPL,
            format!(
                "`{}->{}` does not match its declaration in the trait `{}`: {difference}",
                def.name.value, own_fn.name.value, module.traits[tr].name.value
            ),
            own_fn.name.span,
        )
        .with_help(format!(
            "declare it as `{}`",
            signature(required_fn, &subst)
        ));
        self.report(at_declaration(
            diagnostic,
            module,
            tr,
            required_fn,
            "declared here",
        ));
    }
}

/// Points `diagnostic` at the declaration of `function` in trait `tr`, unless the trait is a
/// prelude trait, which is not declared in the source.
fn at_declaration(
    diagnostic: Diagnostic,
    module: &Module,
    tr: TraitId,
    function: &Function,
    message: &str,
) -> Diagnostic {
    if module.traits[tr].prelude.is_some() {
        return diagnostic;
    }
    diagnostic.with_secondary(function.name.span, message)
}

/// How the signature of `own` differs from that of `required` with the types `subst`, if it
/// does.
fn signature_difference(required: &Function, own: &Function, subst: &[Ty]) -> Option<String> {
    let required_own = &required.generics.params[required.own_generics..];
    let own_own = &own.generics.params[own.own_generics..];
    if required_own.len() != own_own.len() {
        return Some(format!(
            "it has {} type parameters instead of {}",
            own_own.len(),
            required_own.len()
        ));
    }
    for (expected, actual) in required_own.iter().zip(own_own) {
        let same = expected.bounds.len() == actual.bounds.len()
            && expected
                .bounds
                .iter()
                .all(|b| actual.bounds.iter().any(|a| a.value == b.value));
        if !same {
            return Some(format!(
                "the type parameter `{}` has different bounds",
                actual.name.value
            ));
        }
    }
    if required.params.len() != own.params.len() {
        return Some(format!(
            "it has {} parameters instead of {}",
            own.params.len(),
            required.params.len()
        ));
    }
    for (index, (expected, actual)) in required.params.iter().zip(&own.params).enumerate() {
        if expected.is_self != actual.is_self {
            return Some(if expected.is_self {
                "it is not a method: its first parameter is not `self`".to_owned()
            } else {
                "it is a method, but the trait's function is not".to_owned()
            });
        }
        if expected.convention != actual.convention {
            return Some(format!(
                "parameter {} is passed {} instead of {}",
                index + 1,
                convention_name(actual.convention),
                convention_name(expected.convention)
            ));
        }
        let expected_ty = expected.ty.value.subst(subst);
        if expected_ty != actual.ty.value {
            return Some(format!(
                "parameter {} has type `{}` instead of `{expected_ty}`",
                index + 1,
                actual.ty.value
            ));
        }
    }
    if required.raises != own.raises {
        return Some(if required.raises {
            "the trait's function is declared `raises`, but it is not".to_owned()
        } else {
            "it is declared `raises`, but the trait's function is not".to_owned()
        });
    }
    let expected_ret = required.ret.value.subst(subst);
    if expected_ret != own.ret.value {
        return Some(format!(
            "it returns `{}` instead of `{expected_ret}`",
            own.ret.value
        ));
    }
    None
}

fn convention_name(convention: Convention) -> &'static str {
    match convention {
        Convention::Read => "by `read`",
        Convention::Mut => "as `mut`",
        Convention::Owned => "as `owned`",
    }
}

/// The signature of a trait's function for the types `subst`, as written in a declaration.
fn signature(function: &Function, subst: &[Ty]) -> String {
    let mut text = format!(":fn {}", function.name.value);
    let own = &function.generics.params[function.own_generics..];
    if !own.is_empty() {
        let names: Vec<&str> = own.iter().map(|p| p.name.value.as_str()).collect();
        write!(text, "<{}>", names.join(", ")).expect("writing to a String");
    }
    for param in &function.params {
        text.push(' ');
        match param.convention {
            Convention::Mut => text.push_str("mut "),
            Convention::Owned => text.push_str("owned "),
            Convention::Read => {}
        }
        if param.is_self {
            text.push_str("self");
        } else {
            let name = &function.body.locals[param.local].name.value;
            write!(text, "{name}:{}", param.ty.value.subst(subst)).expect("writing to a String");
        }
    }
    let ret = function.ret.value.subst(subst);
    if ret != Ty::Nothing {
        write!(text, " -> {ret}").expect("writing to a String");
    }
    if function.raises {
        text.push_str(" raises");
    }
    text
}

/// Whether a type derives a built-in trait, or one that implies it.
fn derives(derives: &Derives, builtin: BuiltinTrait) -> bool {
    match builtin {
        BuiltinTrait::Copy => derives.copy.is_some(),
        BuiltinTrait::Clone => derives.clone.is_some() || derives.copy.is_some(),
        BuiltinTrait::Eq => derives.eq.is_some() || derives.ord.is_some(),
        BuiltinTrait::Ord => derives.ord.is_some(),
        BuiltinTrait::Hash => derives.hash.is_some(),
        BuiltinTrait::Display => derives.display.is_some(),
        BuiltinTrait::Default => derives.default.is_some(),
    }
}

/// A trait's name, for messages.
fn bound_name(module: &Module, bound: Bound) -> String {
    match bound {
        Bound::Builtin(builtin) => builtin.name().to_owned(),
        Bound::Trait(id) => module.traits[id].name.value.clone(),
    }
}
