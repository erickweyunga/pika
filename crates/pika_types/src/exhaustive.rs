//! Exhaustiveness and reachability of `:match` arms.
//!
//! The check follows the "usefulness" algorithm of Maranget ("Warnings for pattern
//! matching", 2007): a pattern is useful after some rows if a value matches it and none of
//! the rows. An arm is unreachable if its pattern is not useful after the unguarded arms
//! before it, and a `:match` is exhaustive if a wildcard is not useful after all of its
//! unguarded arms. When it is useful, the algorithm also builds a value that no arm matches,
//! which the error shows.

use pika_hir::{Body, Literal, Module, Pat, PatId, TypeId};
use pika_ty::Ty;

/// A constructor of values: what a pattern tests before its sub-patterns.
#[derive(Clone, Debug, PartialEq)]
enum Ctor {
    Bool(bool),
    None,
    Some,
    Variant(TypeId, usize),
    Int(i128),
    Char(char),
    Str(String),
}

/// A pattern reduced to what matters for the check.
#[derive(Clone, Debug)]
enum DPat {
    Wild,
    Ctor(Ctor, Vec<DPat>),
}

/// The result of checking one `:match`.
pub(crate) struct MatchCheck {
    /// The arms whose patterns no value can reach.
    pub(crate) unreachable: Vec<usize>,
    /// A value that no arm matches, written as a pattern, if the arms are not exhaustive.
    pub(crate) missing: Option<String>,
}

/// Checks the arms of a `:match` on a value of type `ty`. Each arm is its pattern and
/// whether it has a guard.
pub(crate) fn check_match(
    module: &Module,
    body: &Body,
    ty: Ty,
    arms: &[(PatId, bool)],
) -> MatchCheck {
    let cx = Cx { module };
    let mut rows: Vec<Vec<DPat>> = Vec::new();
    let mut unreachable = Vec::new();
    for (index, &(pat, guarded)) in arms.iter().enumerate() {
        let pattern = lower(body, pat);
        if cx
            .useful(&rows, std::slice::from_ref(&pattern), &[ty])
            .is_none()
        {
            unreachable.push(index);
        }
        // A guarded arm may not match, so it does not cover anything.
        if !guarded {
            rows.push(vec![pattern]);
        }
    }
    let missing = cx
        .useful(&rows, &[DPat::Wild], &[ty])
        .map(|witness| cx.show(&witness[0], false));
    MatchCheck {
        unreachable,
        missing,
    }
}

fn lower(body: &Body, pat: PatId) -> DPat {
    match &body.pats[pat] {
        Pat::Invalid(_) | Pat::Wildcard | Pat::Binding(_) => DPat::Wild,
        Pat::None => DPat::Ctor(Ctor::None, Vec::new()),
        &Pat::Some(inner) => DPat::Ctor(Ctor::Some, vec![lower(body, inner)]),
        Pat::Variant {
            ty,
            variant,
            fields,
            ..
        } => DPat::Ctor(
            Ctor::Variant(*ty, *variant),
            fields.iter().map(|&f| lower(body, f)).collect(),
        ),
        Pat::String(text) => DPat::Ctor(Ctor::Str(text.clone()), Vec::new()),
        Pat::Literal(literal) => {
            let ctor = match *literal {
                Literal::Bool(value) => Ctor::Bool(value),
                Literal::Char(c) => Ctor::Char(c),
                Literal::Int { value, negative } => {
                    let magnitude = i128::from(value);
                    Ctor::Int(if negative { -magnitude } else { magnitude })
                }
                // Not valid patterns; already reported.
                Literal::Float(_) | Literal::Duration(_) => return DPat::Wild,
            };
            DPat::Ctor(ctor, Vec::new())
        }
    }
}

struct Cx<'m> {
    module: &'m Module,
}

impl Cx<'_> {
    /// Every constructor of `ty`, or `None` for types with too many values to list.
    fn ctors(&self, ty: Ty) -> Option<Vec<Ctor>> {
        match ty {
            Ty::Bool => Some(vec![Ctor::Bool(false), Ctor::Bool(true)]),
            Ty::Option(_) => Some(vec![Ctor::None, Ctor::Some]),
            Ty::Adt(_) => {
                let id = self.enum_id(ty)?;
                let count = self.module.types[id].variants.len();
                Some((0..count).map(|v| Ctor::Variant(id, v)).collect())
            }
            _ => None,
        }
    }

    fn enum_id(&self, ty: Ty) -> Option<TypeId> {
        let adt = ty.as_enum()?;
        let id = TypeId::from_raw(la_arena::RawIdx::from_u32(adt.index));
        ((adt.index as usize) < self.module.types.len()).then_some(id)
    }

    /// The types of the fields of a constructor of `ty`.
    fn fields(&self, ty: Ty, ctor: &Ctor) -> Vec<Ty> {
        match (ctor, ty) {
            (Ctor::Some, Ty::Option(inner)) => vec![*inner],
            (Ctor::Some, _) => vec![Ty::Error],
            (&Ctor::Variant(id, variant), _) => self.module.types[id].variants[variant]
                .fields
                .iter()
                .map(|f| f.ty.value.subst(&ty.components()))
                .collect(),
            _ => Vec::new(),
        }
    }

    /// A value matched by `q` and by none of `rows`, as one pattern per column, or `None` if
    /// there is none. All rows and `q` have one pattern per type of `tys`.
    fn useful(&self, rows: &[Vec<DPat>], q: &[DPat], tys: &[Ty]) -> Option<Vec<DPat>> {
        let Some((head, rest)) = q.split_first() else {
            return rows.is_empty().then(Vec::new);
        };
        let ty = tys[0];
        match head {
            DPat::Ctor(ctor, _) => self.useful_ctor(rows, q, tys, ctor),
            DPat::Wild => {
                let used: Vec<&Ctor> = rows
                    .iter()
                    .filter_map(|row| match &row[0] {
                        DPat::Ctor(ctor, _) => Some(ctor),
                        DPat::Wild => None,
                    })
                    .collect();
                let all = self.ctors(ty);
                if let Some(all) = &all
                    && all.iter().all(|c| used.contains(&c))
                {
                    // Every constructor appears: the wildcard is useful if it is for one.
                    return all
                        .iter()
                        .find_map(|ctor| self.useful_ctor(rows, q, tys, ctor));
                }
                // Some values of the type match no constructor of the rows: only the rows
                // with a wildcard here can match them.
                let defaults: Vec<Vec<DPat>> = rows
                    .iter()
                    .filter(|row| matches!(row[0], DPat::Wild))
                    .map(|row| row[1..].to_vec())
                    .collect();
                let witness = self.useful(&defaults, rest, &tys[1..])?;
                let missing_head = all
                    .and_then(|all| all.into_iter().find(|c| !used.contains(&c)))
                    .map_or(DPat::Wild, |ctor| {
                        let arity = self.fields(ty, &ctor).len();
                        DPat::Ctor(ctor, vec![DPat::Wild; arity])
                    });
                Some(std::iter::once(missing_head).chain(witness).collect())
            }
        }
    }

    /// [`Cx::useful`] for values built with `ctor` in the first column.
    fn useful_ctor(
        &self,
        rows: &[Vec<DPat>],
        q: &[DPat],
        tys: &[Ty],
        ctor: &Ctor,
    ) -> Option<Vec<DPat>> {
        let field_tys = self.fields(tys[0], ctor);
        let arity = field_tys.len();
        let specialized: Vec<Vec<DPat>> = rows
            .iter()
            .filter_map(|row| specialize(row, ctor, arity))
            .collect();
        let q = specialize(q, ctor, arity).expect("`q` starts with `ctor` or a wildcard");
        let tys: Vec<Ty> = field_tys
            .into_iter()
            .chain(tys[1..].iter().copied())
            .collect();
        let mut witness = self.useful(&specialized, &q, &tys)?;
        let rest = witness.split_off(arity);
        Some(
            std::iter::once(DPat::Ctor(ctor.clone(), witness))
                .chain(rest)
                .collect(),
        )
    }

    /// A pattern as written in source. Sub-patterns with fields of their own need
    /// parentheses (`nested`).
    fn show(&self, pat: &DPat, nested: bool) -> String {
        let DPat::Ctor(ctor, fields) = pat else {
            return "_".to_owned();
        };
        let head = match ctor {
            Ctor::Bool(value) => value.to_string(),
            Ctor::None => "none".to_owned(),
            Ctor::Some => "some".to_owned(),
            &Ctor::Variant(id, variant) => {
                let def = &self.module.types[id];
                format!("{}->{}", def.name.value, def.variants[variant].name.value)
            }
            Ctor::Int(value) => value.to_string(),
            Ctor::Char(c) => format!("{c:?}"),
            Ctor::Str(text) => format!("{text:?}"),
        };
        if fields.is_empty() {
            return head;
        }
        let parts: Vec<String> = fields.iter().map(|f| self.show(f, true)).collect();
        let text = format!("{head} {}", parts.join(" "));
        if nested { format!("({text})") } else { text }
    }
}

/// The row without its first pattern, which must match values built with `ctor`: its
/// sub-patterns take its place. `None` if the row cannot match such values.
fn specialize(row: &[DPat], ctor: &Ctor, arity: usize) -> Option<Vec<DPat>> {
    let (head, rest) = row.split_first().expect("rows are not empty");
    let fields = match head {
        DPat::Wild => vec![DPat::Wild; arity],
        DPat::Ctor(c, fields) if c == ctor => fields.clone(),
        DPat::Ctor(..) => return None,
    };
    Some(fields.into_iter().chain(rest.iter().cloned()).collect())
}
