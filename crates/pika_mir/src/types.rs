//! What MIR consumers need to know about user-defined and compound types.
//!
//! User-defined types may have type parameters. Their information is stored once, in terms of
//! the parameters, and substituted with the arguments of the instance asked about.

use std::sync::Arc;

use la_arena::ArenaMap;
use pika_hir::{AdtKind, Bound, FnId, Module, PreludeTrait, TypeId};
use pika_ty::Ty;

use crate::value::StructShape;

/// The names of an enum and its variants, shared by all of its values.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AdtShape {
    /// The enum's type.
    pub ty: Ty,
    /// The enum's name, as displayed: without type arguments.
    pub name: String,
    /// Each variant's name and fields' names. Options have the variants `none` and `some`.
    pub variants: Vec<(String, Vec<String>)>,
}

/// A user-defined type, as seen by MIR consumers.
#[derive(Clone, Debug)]
pub struct AdtInfo {
    /// The type's name.
    pub name: String,
    /// The type, with its own parameters as arguments.
    pub ty: Ty,
    /// Whether it is a struct or an enum.
    pub kind: AdtKind,
    /// The fields of a struct: each field's name and type, in declaration order.
    pub fields: Vec<(String, Ty)>,
    /// The variants of an enum: each variant's name and fields.
    pub variants: Vec<(String, Vec<(String, Ty)>)>,
    /// Whether `Copy` is derived: values are copied if their fields are.
    pub copy: bool,
    /// The type's `drop` function, if it implements `Drop`: it runs when a value is destroyed,
    /// before its fields are.
    pub drop: Option<FnId>,
    /// The type's `fmt` function, if it displays its values itself.
    pub fmt: Option<FnId>,
    /// Whether `Display` is derived: values display with `fmt`, or as literals of their
    /// fields.
    pub display: bool,
    /// Whether this is the `Error` type, whose values display as their message.
    pub is_error: bool,
}

/// User-defined types of a program.
#[derive(Clone, Debug, Default)]
pub struct Types {
    /// Every struct and enum.
    pub adts: ArenaMap<TypeId, AdtInfo>,
}

impl Types {
    /// Collects the types of a module that type checked without errors (in particular, no
    /// type contains itself by value).
    pub fn from_module(module: &Module) -> Self {
        let mut types = Self::default();
        let drop_trait = module
            .traits
            .iter()
            .find(|(_, def)| def.prelude == Some(PreludeTrait::Drop))
            .map(|(id, _)| id);
        for (id, def) in module.types.iter() {
            let implements_drop = drop_trait.is_some_and(|drop_trait| {
                def.traits.iter().any(|t| {
                    pika_types::implied_bounds(module, Bound::Trait(t.value))
                        .contains(&Bound::Trait(drop_trait))
                })
            });
            let fields = def
                .fields
                .iter()
                .map(|f| (f.name.value.clone(), f.ty.value))
                .collect();
            let variants = def
                .variants
                .iter()
                .map(|v| {
                    let fields = v
                        .fields
                        .iter()
                        .map(|f| (f.name.value.clone(), f.ty.value))
                        .collect();
                    (v.name.value.clone(), fields)
                })
                .collect();
            types.adts.insert(
                id,
                AdtInfo {
                    name: def.name.value.clone(),
                    ty: def.ty,
                    kind: def.kind,
                    fields,
                    variants,
                    copy: def.derives.copy.is_some(),
                    drop: def.function("drop").filter(|_| implements_drop),
                    fmt: def
                        .function("fmt")
                        .filter(|_| def.derives.display.is_some()),
                    display: def.derives.display.is_some(),
                    is_error: module.error_type == Some(id),
                },
            );
        }
        types
    }

    /// Returns true if values of `ty` can be displayed, as `:put` displays them.
    pub fn can_display(&self, ty: Ty) -> bool {
        match ty {
            Ty::Int(_) | Ty::Float(_) | Ty::Bool | Ty::Char | Ty::String | Ty::Duration => true,
            Ty::Option(inner) | Ty::List(inner) | Ty::Set(inner) => self.can_display(*inner),
            Ty::Map(map) => self.can_display(map.key) && self.can_display(map.value),
            Ty::Adt(adt) => self.adt(ty).is_some_and(|info| {
                info.display
                    && (info.fmt.is_some()
                        || info.is_error
                        || adt.args.iter().all(|&arg| self.can_display(arg)))
            }),
            _ => false,
        }
    }

    /// The `Error` type.
    pub fn error_ty(&self) -> Ty {
        self.adts
            .values()
            .find(|info| info.is_error)
            .map_or(Ty::Error, |info| info.ty)
    }

    /// The user-defined type a type is, if it is one.
    pub fn adt_id(&self, ty: Ty) -> Option<TypeId> {
        let Ty::Adt(adt) = ty else {
            return None;
        };
        let id = TypeId::from_raw(la_arena::RawIdx::from_u32(adt.index));
        self.adts.get(id).map(|_| id)
    }

    /// The user-defined type a type is, with its information in terms of its parameters.
    pub fn adt(&self, ty: Ty) -> Option<&AdtInfo> {
        self.adt_id(ty).map(|id| &self.adts[id])
    }

    /// The struct a type is, with its information in terms of its parameters.
    pub fn struct_info(&self, ty: Ty) -> Option<&AdtInfo> {
        self.adt(ty).filter(|info| info.kind == AdtKind::Struct)
    }

    /// The fields of struct type `ty`: each field's name and type.
    pub fn fields(&self, ty: Ty) -> Option<Vec<(String, Ty)>> {
        let info = self.struct_info(ty)?;
        let args = ty.components();
        Some(
            info.fields
                .iter()
                .map(|(name, field)| (name.clone(), field.subst(&args)))
                .collect(),
        )
    }

    /// The types of the fields of each variant, for an enum or an option.
    pub fn variants(&self, ty: Ty) -> Option<Vec<Vec<Ty>>> {
        if let Ty::Option(inner) = ty {
            return Some(vec![Vec::new(), vec![*inner]]);
        }
        let info = self.adt(ty).filter(|info| info.kind == AdtKind::Enum)?;
        let args = ty.components();
        Some(
            info.variants
                .iter()
                .map(|(_, fields)| fields.iter().map(|&(_, ty)| ty.subst(&args)).collect())
                .collect(),
        )
    }

    /// The shape of the values of struct type `ty`.
    pub fn struct_shape(&self, ty: Ty) -> Option<Arc<StructShape>> {
        let info = self.struct_info(ty)?;
        Some(Arc::new(StructShape {
            ty,
            name: info.name.clone(),
            displays_first_field: info.is_error,
            field_names: info.fields.iter().map(|(name, _)| name.clone()).collect(),
        }))
    }

    /// The shape of the values of an enum or option type.
    pub fn enum_shape(&self, ty: Ty) -> Option<Arc<AdtShape>> {
        if let Ty::Option(_) = ty {
            return Some(Arc::new(AdtShape {
                ty,
                name: ty.to_string(),
                variants: vec![
                    ("none".to_owned(), Vec::new()),
                    ("some".to_owned(), vec!["value".to_owned()]),
                ],
            }));
        }
        let info = self.adt(ty).filter(|info| info.kind == AdtKind::Enum)?;
        Some(Arc::new(AdtShape {
            ty,
            name: info.name.clone(),
            variants: info
                .variants
                .iter()
                .map(|(name, fields)| {
                    (
                        name.clone(),
                        fields.iter().map(|(n, _)| n.clone()).collect(),
                    )
                })
                .collect(),
        }))
    }

    /// The types of all fields of all variants of a user-defined type, substituted.
    fn all_field_tys(&self, ty: Ty) -> Vec<Ty> {
        let Some(info) = self.adt(ty) else {
            return Vec::new();
        };
        let args = ty.components();
        info.fields
            .iter()
            .chain(info.variants.iter().flat_map(|(_, fields)| fields))
            .map(|&(_, field)| field.subst(&args))
            .collect()
    }

    /// Returns true for types whose values are copied rather than moved (spec section 12.1).
    pub fn is_copy(&self, ty: Ty) -> bool {
        self.is_copy_in(ty, &[])
    }

    /// Like [`Self::is_copy`], in a generic body where `params[i]` tells whether type
    /// parameter `i` is `Copy`.
    pub fn is_copy_in(&self, ty: Ty, params: &[bool]) -> bool {
        match ty {
            Ty::String
            | Ty::Formatter
            | Ty::Box(_)
            | Ty::Fn(_)
            | Ty::List(_)
            | Ty::Map(_)
            | Ty::Set(_) => false,
            Ty::Param(param) => params.get(param.index as usize).copied().unwrap_or(false),
            Ty::Option(inner) => self.is_copy_in(*inner, params),
            // A derived `Copy` holds when the fields are `Copy`. A `Copy` type cannot contain
            // itself (only a box could hold it), so the recursion ends.
            _ => match self.adt(ty) {
                Some(info) => {
                    info.copy
                        && self
                            .all_field_tys(ty)
                            .into_iter()
                            .all(|field| self.is_copy_in(field, params))
                }
                None => true,
            },
        }
    }

    /// Returns true for types whose values may own resources that must be freed.
    pub fn needs_drop(&self, ty: Ty) -> bool {
        self.needs_drop_in(ty, &[])
    }

    /// Like [`Self::needs_drop`], in a generic body where `params[i]` tells whether type
    /// parameter `i` is `Copy`.
    pub fn needs_drop_in(&self, ty: Ty, params: &[bool]) -> bool {
        self.needs_drop_visiting(ty, params, &mut Vec::new())
    }

    /// Types cannot contain themselves except through a box, which needs dropping anyway, so
    /// the recursion ends when a type is visited again.
    fn needs_drop_visiting(&self, ty: Ty, params: &[bool], visiting: &mut Vec<Ty>) -> bool {
        match ty {
            Ty::String
            | Ty::Formatter
            | Ty::Box(_)
            | Ty::Fn(_)
            | Ty::List(_)
            | Ty::Map(_)
            | Ty::Set(_) => true,
            Ty::Param(param) => !params.get(param.index as usize).copied().unwrap_or(false),
            Ty::Option(inner) => self.needs_drop_visiting(*inner, params, visiting),
            Ty::Adt(_) if self.adt(ty).is_some_and(|info| info.drop.is_some()) => true,
            Ty::Adt(_) => {
                if visiting.contains(&ty) {
                    return false;
                }
                visiting.push(ty);
                let result = self
                    .all_field_tys(ty)
                    .into_iter()
                    .any(|field| self.needs_drop_visiting(field, params, visiting));
                visiting.pop();
                result
            }
            _ => false,
        }
    }

    /// The struct and enum types with a `drop` function in a value of type `ty`, including
    /// itself, outermost first.
    pub fn user_drops(&self, ty: Ty) -> Vec<Ty> {
        self.adts_with(ty, |info| info.drop.is_some())
    }

    /// The struct and enum types with a `fmt` function in a value of type `ty`, including
    /// itself, outermost first.
    pub fn user_displays(&self, ty: Ty) -> Vec<Ty> {
        self.adts_with(ty, |info| info.fmt.is_some())
    }

    /// Whether a value of type `ty` may hold function values.
    pub fn holds_functions(&self, ty: Ty) -> bool {
        self.holds_functions_visiting(ty, &mut Vec::new())
    }

    fn holds_functions_visiting(&self, ty: Ty, visiting: &mut Vec<Ty>) -> bool {
        if let Ty::Fn(_) = ty {
            return true;
        }
        if visiting.contains(&ty) {
            return false;
        }
        visiting.push(ty);
        let parts = match ty {
            Ty::Adt(_) => self.all_field_tys(ty),
            other => other.components(),
        };
        let holds = parts
            .into_iter()
            .any(|part| self.holds_functions_visiting(part, visiting));
        visiting.pop();
        holds
    }

    /// The struct and enum types in a value of type `ty` whose information passes `test`.
    fn adts_with(&self, ty: Ty, test: impl Fn(&AdtInfo) -> bool + Copy) -> Vec<Ty> {
        let mut found = Vec::new();
        self.collect_adts(ty, test, &mut found, &mut Vec::new());
        found
    }

    fn collect_adts(
        &self,
        ty: Ty,
        test: impl Fn(&AdtInfo) -> bool + Copy,
        found: &mut Vec<Ty>,
        visiting: &mut Vec<Ty>,
    ) {
        if visiting.contains(&ty) {
            return;
        }
        visiting.push(ty);
        if self.adt(ty).is_some_and(test) && !found.contains(&ty) {
            found.push(ty);
        }
        let parts = match ty {
            Ty::Adt(_) => self.all_field_tys(ty),
            // A function value holds the values it captured, whose types its type does not
            // show; they are found where the function value is created.
            Ty::Fn(_) => Vec::new(),
            other => other.components(),
        };
        for part in parts {
            self.collect_adts(part, test, found, visiting);
        }
    }

    /// The type of field `index` of struct type `ty`.
    pub fn field_ty(&self, ty: Ty, index: u32) -> Ty {
        self.struct_info(ty)
            .and_then(|info| info.fields.get(index as usize))
            .map_or(Ty::Error, |&(_, field)| field.subst(&ty.components()))
    }

    /// The type of field `index` of variant `variant` of enum or option type `ty`.
    pub fn variant_field_ty(&self, ty: Ty, variant: u32, index: u32) -> Ty {
        self.variants(ty)
            .and_then(|variants| {
                variants
                    .get(variant as usize)
                    .and_then(|fields| fields.get(index as usize).copied())
            })
            .unwrap_or(Ty::Error)
    }
}
