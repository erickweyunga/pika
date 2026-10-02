//! Types during and after inference, and the unification table.

use pika_ty::{FloatTy, IntTy, Ty, TyVar};

/// What a type variable may become.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum VarKind {
    /// Any type.
    General,
    /// An integer type: the type of an integer literal. Defaults to `i64`.
    Int,
    /// A floating-point type: the type of a float literal. Defaults to `f64`.
    Float,
}

#[derive(Clone, Copy, Debug)]
enum Entry {
    Unbound(VarKind),
    Bound(Ty),
}

/// Type variables and what they have been unified with.
#[derive(Default)]
pub(crate) struct Table {
    entries: Vec<Entry>,
}

impl Table {
    pub(crate) fn new_var(&mut self, kind: VarKind) -> Ty {
        let index = u32::try_from(self.entries.len()).expect("fewer than 2^32 type variables");
        self.entries.push(Entry::Unbound(kind));
        Ty::Var(TyVar(index))
    }

    /// Follows bindings until reaching a concrete type or an unbound variable.
    pub(crate) fn resolve(&self, mut ty: Ty) -> Ty {
        while let Ty::Var(TyVar(index)) = ty {
            match self.entries[index as usize] {
                Entry::Bound(bound) => ty = bound,
                Entry::Unbound(_) => break,
            }
        }
        ty
    }

    /// The type with every variable inside it resolved as far as possible.
    pub(crate) fn resolve_deep(&self, ty: Ty) -> Ty {
        let ty = self.resolve(ty);
        if matches!(ty, Ty::Var(_)) || !ty.has_vars() {
            return ty;
        }
        // A variable may be bound to a type with variables of its own; the occurs check keeps
        // this from going on forever.
        ty.map(&mut |t| match t {
            Ty::Var(_) => self.resolve_deep(t),
            other => other,
        })
    }

    /// Returns true if `var` occurs in `ty`, so that binding it to `ty` would make an infinite
    /// type.
    fn occurs(&self, var: TyVar, ty: Ty) -> bool {
        match self.resolve(ty) {
            Ty::Var(other) => other == var,
            other => other.components().into_iter().any(|c| self.occurs(var, c)),
        }
    }

    /// The kind of an unbound variable.
    pub(crate) fn kind(&self, var: TyVar) -> VarKind {
        match self.entries[var.0 as usize] {
            Entry::Unbound(kind) => kind,
            Entry::Bound(_) => unreachable!("`kind` is only called on resolved variables"),
        }
    }

    /// Makes `a` and `b` the same type, if possible.
    pub(crate) fn unify(&mut self, a: Ty, b: Ty) -> Result<(), ()> {
        let (a, b) = (self.resolve(a), self.resolve(b));
        match (a, b) {
            // A variable unified with an erroneous type becomes erroneous too, so that the
            // original error is not followed by "cannot infer the type".
            (Ty::Var(var), Ty::Error) | (Ty::Error, Ty::Var(var)) => {
                self.entries[var.0 as usize] = Entry::Bound(Ty::Error);
                Ok(())
            }
            (Ty::Error, _) | (_, Ty::Error) => Ok(()),
            (Ty::Var(x), Ty::Var(y)) if x == y => Ok(()),
            (Ty::Var(x), Ty::Var(y)) => {
                let kind = match (self.kind(x), self.kind(y)) {
                    (VarKind::General, other) | (other, VarKind::General) => other,
                    (VarKind::Int, VarKind::Int) => VarKind::Int,
                    (VarKind::Float, VarKind::Float) => VarKind::Float,
                    (VarKind::Int, VarKind::Float) | (VarKind::Float, VarKind::Int) => {
                        return Err(());
                    }
                };
                self.entries[y.0 as usize] = Entry::Unbound(kind);
                self.entries[x.0 as usize] = Entry::Bound(Ty::Var(y));
                Ok(())
            }
            (Ty::Var(var), ty) | (ty, Ty::Var(var)) => {
                let accepted = match self.kind(var) {
                    VarKind::General => !self.occurs(var, ty),
                    VarKind::Int => matches!(ty, Ty::Int(_) | Ty::Never),
                    VarKind::Float => matches!(ty, Ty::Float(_) | Ty::Never),
                };
                if !accepted {
                    return Err(());
                }
                // `never` converts to any type, so it does not fix a variable.
                if ty != Ty::Never {
                    self.entries[var.0 as usize] = Entry::Bound(ty);
                }
                Ok(())
            }
            // `never` is the type of expressions that do not finish, and fits anywhere.
            (Ty::Never, _) | (_, Ty::Never) => Ok(()),
            (a, b) if a == b => Ok(()),
            (Ty::Option(a), Ty::Option(b))
            | (Ty::Box(a), Ty::Box(b))
            | (Ty::List(a), Ty::List(b))
            | (Ty::Set(a), Ty::Set(b)) => self.unify(*a, *b),
            (Ty::Map(a), Ty::Map(b)) => {
                self.unify(a.key, b.key)?;
                self.unify(a.value, b.value)
            }
            (Ty::Adt(a), Ty::Adt(b))
                if a.kind == b.kind && a.index == b.index && a.args.len() == b.args.len() =>
            {
                for (&x, &y) in a.args.iter().zip(b.args.iter()) {
                    self.unify(x, y)?;
                }
                Ok(())
            }
            (Ty::Fn(a), Ty::Fn(b)) if a.params.len() == b.params.len() && a.raises == b.raises => {
                for (&x, &y) in a.params.iter().zip(b.params.iter()) {
                    self.unify(x, y)?;
                }
                self.unify(a.ret, b.ret)
            }
            _ => Err(()),
        }
    }

    /// Binds every unbound integer variable to `i64` and every float variable to `f64`.
    pub(crate) fn apply_defaults(&mut self) {
        for entry in &mut self.entries {
            match *entry {
                Entry::Unbound(VarKind::Int) => *entry = Entry::Bound(Ty::Int(IntTy::I64)),
                Entry::Unbound(VarKind::Float) => *entry = Entry::Bound(Ty::Float(FloatTy::F64)),
                _ => {}
            }
        }
    }

    /// The type as shown in diagnostics: literal variables are `{integer}` and `{float}`.
    pub(crate) fn display(&self, ty: Ty) -> String {
        match self.resolve(ty) {
            Ty::Var(var) => match self.kind(var) {
                VarKind::General => "_".to_owned(),
                VarKind::Int => "{integer}".to_owned(),
                VarKind::Float => "{float}".to_owned(),
            },
            Ty::Option(inner) => format!("{}?", self.display(*inner)),
            Ty::Box(inner) => format!("Box<{}>", self.display(*inner)),
            Ty::List(inner) => format!("List<{}>", self.display(*inner)),
            Ty::Set(inner) => format!("Set<{}>", self.display(*inner)),
            Ty::Map(map) => format!(
                "Map<{}, {}>",
                self.display(map.key),
                self.display(map.value)
            ),
            Ty::Adt(adt) if !adt.args.is_empty() => {
                let args: Vec<String> = adt.args.iter().map(|&a| self.display(a)).collect();
                format!("{}<{}>", adt.name, args.join(", "))
            }
            Ty::Fn(function) => {
                let params: Vec<String> =
                    function.params.iter().map(|&p| self.display(p)).collect();
                let mut text = format!("fn({})", params.join(", "));
                if self.resolve(function.ret) != Ty::Nothing {
                    text = format!("{text} -> {}", self.display(function.ret));
                }
                if function.raises {
                    text.push_str(" raises");
                }
                text
            }
            resolved => resolved.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_variables_unify_with_matching_types_only() {
        let mut table = Table::default();
        let int = table.new_var(VarKind::Int);
        assert!(table.unify(int, Ty::Float(FloatTy::F64)).is_err());
        assert!(table.unify(int, Ty::Int(IntTy::U8)).is_ok());
        assert_eq!(table.resolve(int), Ty::Int(IntTy::U8));

        let float = table.new_var(VarKind::Float);
        assert!(table.unify(float, Ty::Bool).is_err());
        assert!(table.unify(float, int).is_err());
    }

    #[test]
    fn variables_merge_kinds_and_default() {
        let mut table = Table::default();
        let general = table.new_var(VarKind::General);
        let int = table.new_var(VarKind::Int);
        assert!(table.unify(general, int).is_ok());
        assert_eq!(table.display(general), "{integer}");
        table.apply_defaults();
        assert_eq!(table.resolve(general), Ty::Int(IntTy::I64));
    }

    #[test]
    fn options_unify_structurally() {
        let mut table = Table::default();
        let int = table.new_var(VarKind::Int);
        let general = table.new_var(VarKind::General);
        assert!(
            table
                .unify(Ty::option(int), Ty::option(Ty::Int(IntTy::U8)))
                .is_ok()
        );
        assert_eq!(
            table.resolve_deep(Ty::option(int)),
            Ty::option(Ty::Int(IntTy::U8))
        );
        assert!(
            table
                .unify(Ty::option(Ty::Bool), Ty::boxed(Ty::Bool))
                .is_err()
        );
        // `T = T?` has no solution.
        assert!(table.unify(general, Ty::option(general)).is_err());
        let other = table.new_var(VarKind::Int);
        assert_eq!(
            table.display(Ty::boxed(Ty::option(other))),
            "Box<{integer}?>"
        );
    }

    #[test]
    fn never_and_error_fit_anywhere() {
        let mut table = Table::default();
        assert!(table.unify(Ty::Never, Ty::String).is_ok());
        assert!(table.unify(Ty::Bool, Ty::Error).is_ok());
        let var = table.new_var(VarKind::General);
        assert!(table.unify(var, Ty::Never).is_ok());
        assert!(table.unify(var, Ty::Char).is_ok());
        assert_eq!(table.resolve(var), Ty::Char);
    }
}
