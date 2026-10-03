//! How Pika types are represented in machine code.

use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Arc;

use cranelift_codegen::ir::{AbiParam, Signature, Type, types};
use cranelift_module::Module;
use pika_hir::{AdtKind, FloatTy, IntTy};
use pika_mir::{Body, LocalMode, Types, Value};
use pika_ty::FnTy;
use pika_types::Ty;

/// How values of a type are stored.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Repr {
    /// A machine value of the given type.
    Scalar(Type),
    /// Bytes in memory, handled through their address.
    Memory {
        /// Size in bytes.
        size: u32,
        /// Alignment in bytes, a power of two.
        align: u8,
    },
    /// No runtime representation (`nothing`, `never`, structs without fields).
    None,
}

impl Repr {
    /// The size and alignment of a value stored in memory, such as a field.
    pub(crate) fn size_align(self) -> (u32, u8) {
        match self {
            Self::Scalar(ty) => (ty.bytes(), u8::try_from(ty.bytes()).expect("small scalars")),
            Self::Memory { size, align } => (size, align),
            Self::None => (0, 1),
        }
    }
}

/// The size of a `String` and of a `List`: pointer, length and capacity.
pub(crate) const STRING_SIZE: u32 = 24;
/// The size of a `Map` or `Set`: its entries (a list) and its index table (pointer and
/// number of slots).
pub(crate) const MAP_SIZE: u32 = 40;
/// The byte offset of the length of a string, list, map or set.
pub(crate) const LEN_OFFSET: i32 = 8;

/// How values of a built-in type are stored.
fn primitive(ty: Ty) -> Repr {
    match ty {
        Ty::Int(IntTy::I8 | IntTy::U8) | Ty::Bool => Repr::Scalar(types::I8),
        Ty::Int(IntTy::I16 | IntTy::U16) => Repr::Scalar(types::I16),
        Ty::Int(IntTy::I32 | IntTy::U32) | Ty::Char => Repr::Scalar(types::I32),
        Ty::Int(IntTy::I64 | IntTy::U64) | Ty::Duration => Repr::Scalar(types::I64),
        Ty::Float(FloatTy::F32) => Repr::Scalar(types::F32),
        Ty::Float(FloatTy::F64) => Repr::Scalar(types::F64),
        Ty::String | Ty::Formatter | Ty::List(_) => Repr::Memory {
            size: STRING_SIZE,
            align: 8,
        },
        Ty::Map(_) | Ty::Set(_) => Repr::Memory {
            size: MAP_SIZE,
            align: 8,
        },
        Ty::Nothing
        | Ty::Never
        | Ty::Adt(_)
        | Ty::Option(_)
        | Ty::Box(_)
        | Ty::Fn(_)
        | Ty::Param(_)
        | Ty::Var(_)
        | Ty::Error => Repr::None,
    }
}

/// The machine type of a built-in scalar type.
pub(crate) fn scalar(ty: Ty) -> Option<Type> {
    match primitive(ty) {
        Repr::Scalar(ty) => Some(ty),
        _ => None,
    }
}

pub(crate) fn int_type(int: IntTy) -> Type {
    scalar(Ty::Int(int)).expect("integers are scalars")
}

/// The memory layout of a value with parts: a struct, an enum or an option.
///
/// Structs lay out their fields in declaration order, as a C compiler would. Enums and
/// options start with a `u32` tag, the index of the variant; the fields of every variant
/// follow at the same payload offset, laid out like a struct.
#[derive(Clone, Debug)]
pub(crate) struct Layout {
    /// The type's name: a struct's or an enum's.
    pub(crate) name: String,
    /// The representation of the whole value.
    pub(crate) repr: Repr,
    /// The parts.
    pub(crate) kind: LayoutKind,
}

/// A field in a [`Layout`]: its name, type and byte offset from the start of the value.
pub(crate) type FieldLayout = (String, Ty, u32);

/// A variant in a [`Layout`]: its name and fields.
pub(crate) type VariantLayout = (String, Vec<FieldLayout>);

/// The parts of a [`Layout`].
#[derive(Clone, Debug)]
pub(crate) enum LayoutKind {
    /// A struct and its fields.
    Struct(Vec<FieldLayout>),
    /// An enum or option and its variants.
    Enum(Vec<VariantLayout>),
}

impl Layout {
    /// The fields of a struct (none for an enum).
    pub(crate) fn fields(&self) -> &[FieldLayout] {
        match &self.kind {
            LayoutKind::Struct(fields) => fields,
            LayoutKind::Enum(_) => &[],
        }
    }

    /// The variants of an enum (none for a struct).
    pub(crate) fn variants(&self) -> &[VariantLayout] {
        match &self.kind {
            LayoutKind::Struct(_) => &[],
            LayoutKind::Enum(variants) => variants,
        }
    }
}

/// The size of the tag of enums and options.
const TAG_SIZE: u32 = 4;

/// Lays out fields one after the other from `start`; returns them and where they end, and
/// raises `align` to the largest alignment among them.
fn place_fields(fields: &[FieldRepr], start: u32, align: &mut u8) -> (Vec<FieldLayout>, u32) {
    let mut offset = start;
    let mut placed = Vec::with_capacity(fields.len());
    for (name, ty, repr) in fields {
        let (size, field_align) = repr.size_align();
        offset = offset.next_multiple_of(u32::from(field_align));
        placed.push((name.clone(), *ty, offset));
        offset += size;
        *align = (*align).max(field_align);
    }
    (placed, offset)
}

/// A field before layout: its name, type and representation.
type FieldRepr = (String, Ty, Repr);

/// Lays out an enum or option from the representations of its variants' fields.
fn enum_layout(name: String, variants: Vec<(String, Vec<FieldRepr>)>) -> Layout {
    let mut align = 4u8;
    let payload_align = variants
        .iter()
        .flat_map(|(_, fields)| fields)
        .map(|(_, _, repr)| repr.size_align().1)
        .max()
        .unwrap_or(1);
    let payload = TAG_SIZE.next_multiple_of(u32::from(payload_align));
    let mut end = TAG_SIZE;
    let placed = variants
        .into_iter()
        .map(|(variant, fields)| {
            let (fields, variant_end) = place_fields(&fields, payload, &mut align);
            end = end.max(variant_end);
            (variant, fields)
        })
        .collect();
    Layout {
        name,
        repr: Repr::Memory {
            size: end.next_multiple_of(u32::from(align)),
            align,
        },
        kind: LayoutKind::Enum(placed),
    }
}

/// The layout of an entry of a map or set.
#[derive(Clone, Copy, Debug)]
pub(crate) struct EntryLayout {
    /// The type of the keys.
    pub(crate) key: Ty,
    /// The type of the values (`nothing` for a set).
    pub(crate) value: Ty,
    /// Where the key is.
    pub(crate) key_offset: u32,
    /// Where the value is.
    pub(crate) value_offset: u32,
    /// The size of an entry.
    pub(crate) size: u32,
    /// The alignment of entries.
    pub(crate) align: u8,
}

/// The representation of every type of a program.
pub(crate) struct Layouts {
    types: Types,
    /// The layouts of the struct and enum types met so far. Each instance of a generic type
    /// has its own.
    adts: RefCell<HashMap<Ty, Arc<Layout>>>,
}

impl Layouts {
    pub(crate) fn new(types: &Types) -> Self {
        Self {
            types: types.clone(),
            adts: RefCell::default(),
        }
    }

    /// The layout of a struct or enum type, laying out the types it contains by value
    /// first. Those cannot contain it in turn, so the recursion ends.
    fn adt_layout(&self, ty: Ty) -> Option<Arc<Layout>> {
        if let Some(layout) = self.adts.borrow().get(&ty) {
            return Some(layout.clone());
        }
        let info = self.types.adt(ty)?;
        let reprs = |fields: Vec<(String, Ty)>| -> Vec<FieldRepr> {
            fields
                .into_iter()
                .map(|(name, ty)| (name, ty, self.repr(ty)))
                .collect()
        };
        let layout = match info.kind {
            AdtKind::Struct => {
                let fields = reprs(self.types.fields(ty)?);
                let mut align = 1u8;
                let (placed, end) = place_fields(&fields, 0, &mut align);
                let size = end.next_multiple_of(u32::from(align));
                Layout {
                    name: info.name.clone(),
                    repr: if size == 0 {
                        Repr::None
                    } else {
                        Repr::Memory { size, align }
                    },
                    kind: LayoutKind::Struct(placed),
                }
            }
            AdtKind::Enum => {
                let variants = info
                    .variants
                    .iter()
                    .zip(self.types.variants(ty)?)
                    .map(|((name, fields), tys)| {
                        let fields = fields.iter().map(|(n, _)| n.clone()).zip(tys).collect();
                        (name.clone(), reprs(fields))
                    })
                    .collect();
                enum_layout(info.name.clone(), variants)
            }
        };
        let layout = Arc::new(layout);
        self.adts.borrow_mut().insert(ty, layout.clone());
        Some(layout)
    }

    /// How values of `ty` are stored.
    pub(crate) fn repr(&self, ty: Ty) -> Repr {
        match ty {
            // A box, or a function value: a pointer to the heap.
            Ty::Box(_) | Ty::Fn(_) => Repr::Scalar(types::I64),
            Ty::Option(_) | Ty::Adt(_) => self.layout(ty).map_or(Repr::None, |layout| layout.repr),
            _ => primitive(ty),
        }
    }

    /// The layout of a struct, enum or option type.
    pub(crate) fn layout(&self, ty: Ty) -> Option<Arc<Layout>> {
        if let Ty::Option(inner) = ty {
            let fields = vec![("value".to_owned(), *inner, self.repr(*inner))];
            return Some(Arc::new(enum_layout(
                String::new(),
                vec![("none".to_owned(), Vec::new()), ("some".to_owned(), fields)],
            )));
        }
        self.adt_layout(ty)
    }

    /// The offset of field `index` of struct type `ty`.
    pub(crate) fn field_offset(&self, ty: Ty, index: u32) -> Option<u32> {
        let layout = self.layout(ty)?;
        layout
            .fields()
            .get(index as usize)
            .map(|&(_, _, offset)| offset)
    }

    /// The offset of field `index` of variant `variant` of enum or option type `ty`.
    pub(crate) fn variant_field_offset(&self, ty: Ty, variant: u32, index: u32) -> Option<u32> {
        let layout = self.layout(ty)?;
        let (_, fields) = layout.variants().get(variant as usize)?;
        fields.get(index as usize).map(|&(_, _, offset)| offset)
    }

    /// The size and alignment of values of `ty` in memory.
    pub(crate) fn size_align(&self, ty: Ty) -> (u32, u8) {
        self.repr(ty).size_align()
    }

    /// The layout of an entry of a map or set type: a `u64` hash, then the key, then the
    /// value (nothing for a set).
    pub(crate) fn entry(&self, ty: Ty) -> EntryLayout {
        let (key, value) = match ty {
            Ty::Map(map) => (map.key, map.value),
            Ty::Set(element) => (*element, Ty::Nothing),
            other => unreachable!("not a map: {other}"),
        };
        let (key_size, key_align) = self.size_align(key);
        let (value_size, value_align) = self.size_align(value);
        let key_offset = 8u32.next_multiple_of(u32::from(key_align));
        let value_offset = (key_offset + key_size).next_multiple_of(u32::from(value_align));
        let align = key_align.max(value_align).max(8);
        EntryLayout {
            key,
            value,
            key_offset,
            value_offset,
            size: (value_offset + value_size).next_multiple_of(u32::from(align)),
            align,
        }
    }

    /// Whether `ty` is the `Error` type, whose values display as their message.
    pub(crate) fn is_error(&self, ty: Ty) -> bool {
        self.types.adt(ty).is_some_and(|info| info.is_error)
    }

    /// Returns true for types whose values own resources that must be freed.
    pub(crate) fn needs_drop(&self, ty: Ty) -> bool {
        self.types.needs_drop(ty)
    }

    /// Whether a function returns its value through a hidden destination pointer.
    pub(crate) fn returns_in_memory(&self, body: &Body) -> bool {
        matches!(
            self.repr(body.locals[body.return_local].ty),
            Repr::Memory { .. }
        )
    }

    /// The signature of a function. Parameters held by reference or in memory are pointers;
    /// a result in memory is written through a pointer passed as the first parameter.
    pub(crate) fn signature(&self, module: &dyn Module, body: &Body) -> Signature {
        let pointer = module.target_config().pointer_type();
        let mut signature = module.make_signature();
        // The body of a function value receives its object first.
        if body.env.is_some() {
            signature.params.push(AbiParam::new(pointer));
        }
        if self.returns_in_memory(body) {
            signature.params.push(AbiParam::new(pointer));
        }
        for &param in &body.params {
            let decl = &body.locals[param];
            match (decl.mode, self.repr(decl.ty)) {
                (LocalMode::Ref { .. }, _) | (LocalMode::Value, Repr::Memory { .. }) => {
                    signature.params.push(AbiParam::new(pointer));
                }
                (LocalMode::Value, Repr::Scalar(ty)) => signature.params.push(AbiParam::new(ty)),
                (LocalMode::Value, Repr::None) => {}
            }
        }
        // A function that raises receives where to put its error: an `Error?` that the
        // caller sets to `none`.
        if body.raises {
            signature.params.push(AbiParam::new(pointer));
        }
        if let Repr::Scalar(ty) = self.repr(body.locals[body.return_local].ty) {
            signature.returns.push(AbiParam::new(ty));
        }
        signature
    }

    /// The signature of calls of function values of type `function`: as the
    /// [`Self::signature`] of their bodies.
    pub(crate) fn value_signature(&self, module: &dyn Module, function: &FnTy) -> Signature {
        let pointer = module.target_config().pointer_type();
        let mut signature = module.make_signature();
        signature.params.push(AbiParam::new(pointer));
        if let Repr::Memory { .. } = self.repr(function.ret) {
            signature.params.push(AbiParam::new(pointer));
        }
        for &param in &function.params {
            // Parameters are borrowed: by address, unless they are `Copy`.
            match self.repr(param) {
                _ if !self.types.is_copy(param) => signature.params.push(AbiParam::new(pointer)),
                Repr::Memory { .. } => signature.params.push(AbiParam::new(pointer)),
                Repr::Scalar(ty) => signature.params.push(AbiParam::new(ty)),
                Repr::None => {}
            }
        }
        if function.raises {
            signature.params.push(AbiParam::new(pointer));
        }
        if let Repr::Scalar(ty) = self.repr(function.ret) {
            signature.returns.push(AbiParam::new(ty));
        }
        signature
    }

    /// Where each captured value of the types `captures` is in a function value object,
    /// and the size of the object.
    pub(crate) fn env_layout(&self, captures: &[Ty]) -> (Vec<u32>, u32) {
        let mut offset = crate::functions::CAPTURES;
        let mut offsets = Vec::with_capacity(captures.len());
        for &ty in captures {
            let (size, align) = self.size_align(ty);
            offset = offset.next_multiple_of(u32::from(align));
            offsets.push(offset);
            offset += size;
        }
        (offsets, offset.next_multiple_of(8))
    }

    /// Encodes a constant into `bytes` at `offset`, in its native representation. Strings
    /// refer to static text (capacity 0); their text is returned with the offset of the
    /// pointer to it, for the caller to relocate.
    pub(crate) fn encode(
        &self,
        value: &Value,
        bytes: &mut [u8],
        offset: usize,
        texts: &mut Vec<(usize, String)>,
    ) {
        let mut put = |data: &[u8]| bytes[offset..offset + data.len()].copy_from_slice(data);
        match value {
            #[allow(
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "keeping the low bits"
            )]
            Value::Int { value, ty } => {
                let data = (*value as u64).to_ne_bytes();
                put(&data[..(ty.bits() / 8) as usize]);
            }
            #[allow(
                clippy::cast_possible_truncation,
                reason = "f32 values are stored exactly"
            )]
            Value::Float {
                value,
                ty: FloatTy::F32,
            } => put(&(*value as f32).to_ne_bytes()),
            Value::Float { value, .. } => put(&value.to_ne_bytes()),
            Value::Bool(value) => put(&[u8::from(*value)]),
            Value::Char(value) => put(&u32::from(*value).to_ne_bytes()),
            Value::Duration(nanos) => put(&nanos.to_ne_bytes()),
            Value::Nothing => {}
            Value::Str(text) => {
                // The pointer is relocated; the capacity stays 0.
                put(&[0; 8]);
                bytes[offset + 8..offset + 16].copy_from_slice(&(text.len() as u64).to_ne_bytes());
                texts.push((offset, text.to_string()));
            }
            Value::Struct { shape, fields } => {
                let layout = self.layout(shape.ty).expect("struct values have layouts");
                for (field, &(_, _, field_offset)) in fields.iter().zip(layout.fields()) {
                    self.encode(field, bytes, offset + field_offset as usize, texts);
                }
            }
            Value::Variant {
                shape,
                variant,
                fields,
            } => {
                put(&variant.to_ne_bytes());
                let layout = self.layout(shape.ty).expect("enum values have layouts");
                let (_, field_layouts) = &layout.variants()[*variant as usize];
                for (field, &(_, _, field_offset)) in fields.iter().zip(field_layouts) {
                    self.encode(field, bytes, offset + field_offset as usize, texts);
                }
            }
            // An empty collection is all zeros: no buffer.
            Value::List { elements, .. } if elements.is_empty() => {}
            Value::Map { entries, .. } if entries.is_empty() => {}
            Value::Boxed(_) | Value::List { .. } | Value::Map { .. } | Value::Function(_) => {
                unreachable!("heap values are not constants")
            }
        }
    }
}
