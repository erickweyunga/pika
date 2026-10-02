//! Values and the reference semantics of every MIR operation.
//!
//! The interpreter evaluates operations with these functions, and the code generator must
//! produce the same results; the differential tests compare the two.

use std::cmp::Ordering;

use std::sync::Arc;

use pika_hir::{FloatTy, IntTy};
use pika_runtime::PanicKind;
use pika_runtime::format::{self, RaisedError};
use pika_types::Ty;

use crate::types::AdtShape;
use crate::{BinaryOp, CastKind, UnaryOp, unsigned_of};

/// A runtime or compile-time value.
#[derive(Clone, Debug, PartialEq)]
pub enum Value {
    /// An integer, always within the range of its type.
    Int {
        /// The value.
        value: i128,
        /// Its type.
        ty: IntTy,
    },
    /// A float. `f32` values are stored exactly as `f64`.
    Float {
        /// The value.
        value: f64,
        /// Its type.
        ty: FloatTy,
    },
    /// A `bool`.
    Bool(bool),
    /// A `char`.
    Char(char),
    /// A `Duration`, in nanoseconds.
    Duration(i64),
    /// The value of type `nothing`.
    Nothing,
    /// A `String`.
    Str(Arc<str>),
    /// A struct.
    Struct {
        /// The struct's type and field names.
        shape: Arc<StructShape>,
        /// The fields' values, in declaration order.
        fields: Vec<Value>,
    },
    /// A value of an enum or option: a variant and the values of its fields.
    Variant {
        /// The type and the names of its variants and fields.
        shape: Arc<AdtShape>,
        /// The variant's index.
        variant: u32,
        /// The fields' values, in declaration order.
        fields: Vec<Value>,
    },
    /// A box and the value it holds.
    Boxed(Box<Value>),
    /// A list.
    List {
        /// The list type.
        ty: Ty,
        /// The elements, in order.
        elements: Vec<Value>,
    },
    /// A map, or a set (whose values are `nothing`): entries in insertion order, with
    /// distinct keys.
    Map {
        /// The map or set type.
        ty: Ty,
        /// The keys and values.
        entries: Vec<(Value, Value)>,
    },
    /// A function value. Its copies share it, as they share the captured values.
    Function(Arc<FunctionValue>),
}

/// What a function value is: its body, and the values it captured.
#[derive(Debug, PartialEq)]
pub struct FunctionValue {
    /// The function value's type.
    pub ty: Ty,
    /// The body.
    pub func: crate::InstanceId,
    /// The captured values, in order.
    pub captures: Vec<Value>,
}

/// The type and field names of a struct value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StructShape {
    /// The struct's type.
    pub ty: Ty,
    /// The struct's name, as displayed: without type arguments.
    pub name: String,
    /// Whether values display as their first field, as `Error` displays its message.
    pub displays_first_field: bool,
    /// The fields' names, in declaration order.
    pub field_names: Vec<String>,
}

/// The smallest and largest value of an integer type.
fn int_range(ty: IntTy) -> (i128, i128) {
    let bits = ty.bits();
    if ty.is_signed() {
        (-(1i128 << (bits - 1)), (1i128 << (bits - 1)) - 1)
    } else {
        (0, (1i128 << bits) - 1)
    }
}

/// The two's complement bit pattern of `value` in `ty`'s width.
fn to_bits(value: i128, ty: IntTy) -> u64 {
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "keeping the low bits"
    )]
    let bits = value as u64;
    mask(bits, ty)
}

fn mask(bits: u64, ty: IntTy) -> u64 {
    if ty.bits() == 64 {
        bits
    } else {
        bits & ((1u64 << ty.bits()) - 1)
    }
}

/// The value of the bit pattern `bits` in `ty`.
fn from_bits(bits: u64, ty: IntTy) -> i128 {
    let bits = mask(bits, ty);
    if ty.is_signed() && bits >> (ty.bits() - 1) & 1 == 1 {
        i128::from(bits) - (1i128 << ty.bits())
    } else {
        i128::from(bits)
    }
}

impl Value {
    /// An integer value, or `None` if it does not fit `ty`.
    pub fn int(value: i128, ty: IntTy) -> Option<Self> {
        let (min, max) = int_range(ty);
        (min..=max)
            .contains(&value)
            .then_some(Self::Int { value, ty })
    }

    /// The value's type.
    pub fn ty(&self) -> Ty {
        match self {
            Self::Int { ty, .. } => Ty::Int(*ty),
            Self::Float { ty, .. } => Ty::Float(*ty),
            Self::Bool(_) => Ty::Bool,
            Self::Char(_) => Ty::Char,
            Self::Duration(_) => Ty::Duration,
            Self::Nothing => Ty::Nothing,
            Self::Str(_) => Ty::String,
            Self::Struct { shape, .. } => shape.ty,
            Self::Variant { shape, .. } => shape.ty,
            Self::Boxed(value) => Ty::boxed(value.ty()),
            Self::List { ty, .. } | Self::Map { ty, .. } => *ty,
            Self::Function(function) => function.ty,
        }
    }

    /// The position of the entry with `key` in a map or set.
    pub fn find_key(entries: &[(Value, Value)], key: &Value) -> Option<usize> {
        entries
            .iter()
            .position(|(k, _)| compare_values(k, key) == Some(Ordering::Equal))
    }

    /// The text `:put` prints for this value.
    pub fn display(&self) -> String {
        self.display_with(&mut |_| None)
    }

    /// Like [`Value::display`], with `custom` giving the text of the values whose type has a
    /// `fmt` function of its own (it returns `None` for the others).
    pub fn display_with(&self, custom: &mut dyn FnMut(&Value) -> Option<String>) -> String {
        if let Some(text) = custom(self) {
            return text;
        }
        match self {
            Self::Int { value, .. } => value.to_string(),
            #[allow(
                clippy::cast_possible_truncation,
                reason = "f32 values are stored exactly"
            )]
            Self::Float {
                value,
                ty: FloatTy::F32,
            } => format::f32_to_string(*value as f32),
            Self::Float {
                value,
                ty: FloatTy::F64,
            } => format::f64_to_string(*value),
            Self::Bool(value) => value.to_string(),
            Self::Char(value) => value.to_string(),
            Self::Duration(nanos) => format::duration_to_string(*nanos),
            Self::Nothing => String::new(),
            Self::Str(text) => text.to_string(),
            Self::Struct { shape, fields } if shape.displays_first_field => fields
                .first()
                .map_or_else(String::new, |message| message.display_with(custom)),
            Self::Struct { shape, fields } => {
                let fields: Vec<String> = shape
                    .field_names
                    .iter()
                    .zip(fields)
                    .map(|(name, value)| format!("{name}={}", value.literal_with(custom)))
                    .collect();
                format!("{}{{{}}}", shape.name, fields.join("; "))
            }
            Self::Variant {
                shape,
                variant,
                fields,
            } => {
                let (name, field_names) = &shape.variants[*variant as usize];
                if let Ty::Option(_) = shape.ty {
                    return match fields.first() {
                        Some(value) => format!("[some {}]", value.literal_with(custom)),
                        None => "none".to_owned(),
                    };
                }
                if fields.is_empty() {
                    return format!("{}->{name}", shape.name);
                }
                let fields: Vec<String> = field_names
                    .iter()
                    .zip(fields)
                    .map(|(name, value)| format!("{name}={}", value.literal_with(custom)))
                    .collect();
                format!("{}->{name}{{{}}}", shape.name, fields.join("; "))
            }
            Self::Boxed(value) => value.display_with(custom),
            Self::List { elements, .. } => {
                let elements: Vec<String> =
                    elements.iter().map(|e| e.literal_with(custom)).collect();
                format!("{{{}}}", elements.join("; "))
            }
            Self::Map { ty, entries } => {
                let entries: Vec<String> = entries
                    .iter()
                    .map(|(key, value)| match ty {
                        Ty::Set(_) => key.literal_with(custom),
                        _ => format!(
                            "{}={}",
                            key.literal_with(custom),
                            value.literal_with(custom)
                        ),
                    })
                    .collect();
                format!("{{{}}}", entries.join("; "))
            }
            Self::Function(function) => function.ty.to_string(),
        }
    }

    /// The value as it would be written in source: like [`Value::display`], except that
    /// strings and characters are quoted. Struct fields are displayed this way.
    pub fn literal(&self) -> String {
        self.literal_with(&mut |_| None)
    }

    /// Like [`Value::literal`], with `custom` as in [`Value::display_with`].
    pub fn literal_with(&self, custom: &mut dyn FnMut(&Value) -> Option<String>) -> String {
        match self {
            Self::Str(text) => format::quote_string(text),
            Self::Char(c) => format::quote_char(*c),
            Self::Boxed(value) => value.literal_with(custom),
            other => other.display_with(custom),
        }
    }

    /// For an `Error` value: the error, then each error that caused it, in order.
    pub fn error_chain(&self) -> Vec<RaisedError> {
        let mut chain = Vec::new();
        let mut current = Some(self);
        while let Some(Self::Struct { fields, .. }) = current {
            let [message, source, file, line, column] = fields.as_slice() else {
                break;
            };
            let number = |value: &Value| match value {
                Value::Int { value, .. } => u32::try_from(*value).unwrap_or(0),
                _ => 0,
            };
            chain.push(RaisedError {
                message: message.display(),
                file: file.display(),
                line: number(line),
                column: number(column),
            });
            current = match source {
                Self::Variant { fields, .. } => match fields.first() {
                    Some(Self::Boxed(cause)) => Some(cause),
                    _ => None,
                },
                _ => None,
            };
        }
        chain
    }

    /// The value of a `bool`.
    pub fn as_bool(&self) -> Option<bool> {
        match self {
            Self::Bool(value) => Some(*value),
            _ => None,
        }
    }
}

/// Compares fields in order: the first pair that differs decides.
fn compare_fields(a: &[Value], b: &[Value]) -> Option<Ordering> {
    for (a, b) in a.iter().zip(b) {
        match compare_values(a, b)? {
            Ordering::Equal => {}
            other => return Some(other),
        }
    }
    Some(Ordering::Equal)
}

/// Computes a float operation in the precision of `ty`.
#[allow(
    clippy::cast_possible_truncation,
    reason = "f32 values are stored exactly"
)]
fn float_op(
    ty: FloatTy,
    a: f64,
    b: f64,
    op: impl Fn(f64, f64) -> f64,
    op32: impl Fn(f32, f32) -> f32,
) -> f64 {
    match ty {
        FloatTy::F64 => op(a, b),
        FloatTy::F32 => f64::from(op32(a as f32, b as f32)),
    }
}

/// Compares two values of the same type: field by field, in declaration order, for structs;
/// by variant, then field by field, for enums and options (`none` is smallest); by the
/// value held, for boxes. `None` when the values are unordered, because of a NaN.
fn compare_values(a: &Value, b: &Value) -> Option<Ordering> {
    match (a, b) {
        (Value::Int { value: a, .. }, Value::Int { value: b, .. }) => a.partial_cmp(b),
        (Value::Float { value: a, .. }, Value::Float { value: b, .. }) => a.partial_cmp(b),
        (Value::Bool(a), Value::Bool(b)) => a.partial_cmp(b),
        (Value::Char(a), Value::Char(b)) => a.partial_cmp(b),
        (Value::Duration(a), Value::Duration(b)) => a.partial_cmp(b),
        (Value::Nothing, Value::Nothing) => Some(Ordering::Equal),
        (Value::Str(a), Value::Str(b)) => a.partial_cmp(b),
        (Value::Struct { fields: a, .. }, Value::Struct { fields: b, .. }) => compare_fields(a, b),
        (
            Value::Variant {
                variant: a_variant,
                fields: a,
                ..
            },
            Value::Variant {
                variant: b_variant,
                fields: b,
                ..
            },
        ) => match a_variant.cmp(b_variant) {
            Ordering::Equal => compare_fields(a, b),
            other => Some(other),
        },
        (Value::Boxed(a), Value::Boxed(b)) => compare_values(a, b),
        (Value::List { elements: a, .. }, Value::List { elements: b, .. }) => {
            match compare_fields(a, b)? {
                // A list that is a prefix of the other is smaller.
                Ordering::Equal => Some(a.len().cmp(&b.len())),
                other => Some(other),
            }
        }
        // Maps and sets are equal when they have the same entries, in any order; they have
        // no order otherwise, so unequal ones are reported as `Less`.
        (Value::Map { entries: a, .. }, Value::Map { entries: b, .. }) => {
            if a.len() != b.len() {
                return Some(Ordering::Less);
            }
            for (key, value) in a {
                let Some(index) = Value::find_key(b, key) else {
                    return Some(Ordering::Less);
                };
                if compare_values(value, &b[index].1)? != Ordering::Equal {
                    return Some(Ordering::Less);
                }
            }
            Some(Ordering::Equal)
        }
        _ => unreachable!("comparing values of different types: {a:?}, {b:?}"),
    }
}

fn compare<T: PartialOrd>(op: BinaryOp, a: &T, b: &T) -> Value {
    Value::Bool(match op {
        BinaryOp::Eq => a == b,
        BinaryOp::Ne => a != b,
        BinaryOp::Lt => a < b,
        BinaryOp::Le => a <= b,
        BinaryOp::Gt => a > b,
        BinaryOp::Ge => a >= b,
        _ => unreachable!("not a comparison: {op:?}"),
    })
}

/// Evaluates a binary operation.
///
/// # Errors
///
/// Returns the kind of panic for overflow, division by zero and oversized shifts.
///
/// # Panics
///
/// Panics if the operands do not have the types the type checker guarantees.
pub fn binary(op: BinaryOp, lhs: &Value, rhs: &Value) -> Result<Value, PanicKind> {
    match (lhs, rhs) {
        (
            Value::Int { value: a, ty },
            Value::Int {
                value: b,
                ty: rhs_ty,
            },
        ) => int_binary(op, *a, *b, *ty, *rhs_ty),
        (Value::Float { value: a, ty }, Value::Float { value: b, .. }) => Ok(match op {
            BinaryOp::Add => Value::Float {
                value: float_op(*ty, *a, *b, |x, y| x + y, |x, y| x + y),
                ty: *ty,
            },
            BinaryOp::Sub => Value::Float {
                value: float_op(*ty, *a, *b, |x, y| x - y, |x, y| x - y),
                ty: *ty,
            },
            BinaryOp::Mul => Value::Float {
                value: float_op(*ty, *a, *b, |x, y| x * y, |x, y| x * y),
                ty: *ty,
            },
            BinaryOp::Div => Value::Float {
                value: float_op(*ty, *a, *b, |x, y| x / y, |x, y| x / y),
                ty: *ty,
            },
            BinaryOp::Rem => Value::Float {
                value: float_op(*ty, *a, *b, |x, y| x % y, |x, y| x % y),
                ty: *ty,
            },
            _ => compare(op, a, b),
        }),
        (Value::Duration(a), Value::Duration(b)) => match op {
            BinaryOp::Add => a
                .checked_add(*b)
                .map(Value::Duration)
                .ok_or(PanicKind::Overflow),
            BinaryOp::Sub => a
                .checked_sub(*b)
                .map(Value::Duration)
                .ok_or(PanicKind::Overflow),
            _ => Ok(compare(op, a, b)),
        },
        (Value::Bool(a), Value::Bool(b)) => Ok(compare(op, a, b)),
        (Value::Char(a), Value::Char(b)) => Ok(compare(op, a, b)),
        (Value::Nothing, Value::Nothing) => Ok(compare(op, &(), &())),
        (Value::Str(a), Value::Str(b)) => Ok(match op {
            BinaryOp::Concat => Value::Str(format!("{a}{b}").into()),
            BinaryOp::Contains => Value::Bool(b.contains(&**a)),
            _ => compare(op, a, b),
        }),
        (
            Value::List {
                ty,
                elements: first,
            },
            Value::List {
                elements: second, ..
            },
        ) if op == BinaryOp::Concat => Ok(Value::List {
            ty: *ty,
            elements: first.iter().chain(second).cloned().collect(),
        }),
        (Value::Struct { .. }, Value::Struct { .. })
        | (Value::Variant { .. }, Value::Variant { .. })
        | (Value::Boxed(_), Value::Boxed(_))
        | (Value::List { .. }, Value::List { .. })
        | (Value::Map { .. }, Value::Map { .. }) => {
            let ordering = compare_values(lhs, rhs);
            Ok(Value::Bool(match op {
                BinaryOp::Eq => ordering == Some(Ordering::Equal),
                BinaryOp::Ne => ordering != Some(Ordering::Equal),
                BinaryOp::Lt => ordering == Some(Ordering::Less),
                BinaryOp::Le => matches!(ordering, Some(Ordering::Less | Ordering::Equal)),
                BinaryOp::Gt => ordering == Some(Ordering::Greater),
                BinaryOp::Ge => matches!(ordering, Some(Ordering::Greater | Ordering::Equal)),
                _ => unreachable!("not a struct comparison: {op:?}"),
            }))
        }
        _ => unreachable!("ill-typed operands for {op:?}: {lhs:?}, {rhs:?}"),
    }
}

fn int_binary(
    op: BinaryOp,
    a: i128,
    b: i128,
    ty: IntTy,
    rhs_ty: IntTy,
) -> Result<Value, PanicKind> {
    let checked = |result: i128| Value::int(result, ty).ok_or(PanicKind::Overflow);
    let bits = |result: u64| {
        Ok(Value::Int {
            value: from_bits(result, ty),
            ty,
        })
    };
    match op {
        BinaryOp::Add => checked(a + b),
        BinaryOp::Sub => checked(a - b),
        // The product of two 64-bit values can exceed even `i128`.
        BinaryOp::Mul => a.checked_mul(b).map_or(Err(PanicKind::Overflow), checked),
        BinaryOp::Div | BinaryOp::Rem if b == 0 => Err(PanicKind::DivisionByZero),
        BinaryOp::Div => checked(a / b),
        // `MIN % -1` is 0 (no overflow), unlike `MIN / -1`.
        BinaryOp::Rem => checked(a % b),
        BinaryOp::WrappingAdd => bits(to_bits(a, ty).wrapping_add(to_bits(b, ty))),
        BinaryOp::WrappingSub => bits(to_bits(a, ty).wrapping_sub(to_bits(b, ty))),
        BinaryOp::BitAnd => bits(to_bits(a, ty) & to_bits(b, ty)),
        BinaryOp::BitOr => bits(to_bits(a, ty) | to_bits(b, ty)),
        BinaryOp::BitXor => bits(to_bits(a, ty) ^ to_bits(b, ty)),
        BinaryOp::Shl | BinaryOp::Shr => {
            debug_assert!(Value::int(b, rhs_ty).is_some());
            if b < 0 || b >= i128::from(ty.bits()) {
                return Err(PanicKind::ShiftOverflow);
            }
            let amount = u32::try_from(b).expect("checked above");
            if op == BinaryOp::Shl {
                bits(to_bits(a, ty) << amount)
            } else if ty.is_signed() {
                Ok(Value::Int {
                    value: a >> amount,
                    ty,
                })
            } else {
                bits(to_bits(a, ty) >> amount)
            }
        }
        _ => Ok(compare(op, &a, &b)),
    }
}

/// Evaluates a unary operation.
///
/// # Errors
///
/// Returns [`PanicKind::Overflow`] when negating the smallest value of a type.
///
/// # Panics
///
/// Panics if the operand does not have a type the type checker allows.
pub fn unary(op: UnaryOp, operand: &Value) -> Result<Value, PanicKind> {
    match (op, operand) {
        (UnaryOp::Neg, Value::Int { value, ty }) => {
            Value::int(-value, *ty).ok_or(PanicKind::Overflow)
        }
        (UnaryOp::Neg, Value::Float { value, ty }) => Ok(Value::Float {
            value: -value,
            ty: *ty,
        }),
        (UnaryOp::Neg, Value::Duration(nanos)) => nanos
            .checked_neg()
            .map(Value::Duration)
            .ok_or(PanicKind::Overflow),
        (UnaryOp::Not, Value::Bool(value)) => Ok(Value::Bool(!value)),
        (UnaryOp::BitNot, Value::Int { value, ty }) => Ok(Value::Int {
            value: from_bits(!to_bits(*value, *ty), *ty),
            ty: *ty,
        }),
        _ => unreachable!("ill-typed operand for {op:?}: {operand:?}"),
    }
}

/// Evaluates a conversion.
///
/// # Errors
///
/// Returns [`PanicKind::LossyCast`] for an integer conversion that loses information.
///
/// # Panics
///
/// Panics for conversions the type checker rejects.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_precision_loss,
    clippy::cast_sign_loss,
    reason = "float conversions follow Rust's `as`, which saturates and rounds"
)]
pub fn cast(kind: CastKind, operand: &Value, to: Ty) -> Result<Value, PanicKind> {
    match (operand, to) {
        (Value::Int { value, ty }, Ty::Int(target)) => match kind {
            CastKind::As => Value::int(*value, target).ok_or(PanicKind::LossyCast),
            CastKind::Reinterpret => {
                debug_assert_eq!(unsigned_of(*ty), unsigned_of(target));
                Ok(Value::Int {
                    value: from_bits(to_bits(*value, *ty), target),
                    ty: target,
                })
            }
        },
        (Value::Int { value, .. }, Ty::Float(target)) => Ok(Value::Float {
            value: match target {
                FloatTy::F64 => *value as f64,
                FloatTy::F32 => f64::from(*value as f32),
            },
            ty: target,
        }),
        (Value::Float { value, .. }, Ty::Int(target)) => {
            // Saturating, with NaN converting to 0, like Rust's `as`.
            let value = match target {
                IntTy::I8 => i128::from(*value as i8),
                IntTy::I16 => i128::from(*value as i16),
                IntTy::I32 => i128::from(*value as i32),
                IntTy::I64 => i128::from(*value as i64),
                IntTy::U8 => i128::from(*value as u8),
                IntTy::U16 => i128::from(*value as u16),
                IntTy::U32 => i128::from(*value as u32),
                IntTy::U64 => i128::from(*value as u64),
            };
            Ok(Value::Int { value, ty: target })
        }
        (Value::Float { value, .. }, Ty::Float(target)) => Ok(Value::Float {
            value: match target {
                FloatTy::F64 => *value,
                FloatTy::F32 => f64::from(*value as f32),
            },
            ty: target,
        }),
        (Value::Char(c), Ty::Int(IntTy::U32)) => Ok(Value::Int {
            value: i128::from(u32::from(*c)),
            ty: IntTy::U32,
        }),
        (value, target) if value.ty() == target => Ok(value.clone()),
        _ => unreachable!("invalid cast of {operand:?} to {to}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn int(value: i128, ty: IntTy) -> Value {
        Value::int(value, ty).expect("in range")
    }

    #[test]
    fn checked_arithmetic() {
        assert_eq!(
            binary(BinaryOp::Add, &int(127, IntTy::I8), &int(1, IntTy::I8)),
            Err(PanicKind::Overflow)
        );
        assert_eq!(
            binary(BinaryOp::Sub, &int(0, IntTy::U8), &int(1, IntTy::U8)),
            Err(PanicKind::Overflow)
        );
        assert_eq!(
            binary(BinaryOp::Div, &int(-128, IntTy::I8), &int(-1, IntTy::I8)),
            Err(PanicKind::Overflow)
        );
        assert_eq!(
            binary(BinaryOp::Rem, &int(-128, IntTy::I8), &int(-1, IntTy::I8)),
            Ok(int(0, IntTy::I8))
        );
        assert_eq!(
            binary(BinaryOp::Rem, &int(-7, IntTy::I64), &int(2, IntTy::I64)),
            Ok(int(-1, IntTy::I64))
        );
        assert_eq!(
            binary(BinaryOp::Div, &int(-7, IntTy::I64), &int(2, IntTy::I64)),
            Ok(int(-3, IntTy::I64))
        );
        assert_eq!(
            binary(BinaryOp::Div, &int(1, IntTy::I64), &int(0, IntTy::I64)),
            Err(PanicKind::DivisionByZero)
        );
        let max = int(i128::from(u64::MAX), IntTy::U64);
        assert_eq!(binary(BinaryOp::Mul, &max, &max), Err(PanicKind::Overflow));
        let min = int(i128::from(i64::MIN), IntTy::I64);
        assert_eq!(binary(BinaryOp::Mul, &min, &min), Err(PanicKind::Overflow));
    }

    #[test]
    fn bit_operations() {
        assert_eq!(
            binary(
                BinaryOp::WrappingAdd,
                &int(255, IntTy::U8),
                &int(2, IntTy::U8)
            ),
            Ok(int(1, IntTy::U8))
        );
        assert_eq!(
            binary(BinaryOp::Shl, &int(1, IntTy::I8), &int(7, IntTy::I64)),
            Ok(int(-128, IntTy::I8))
        );
        assert_eq!(
            binary(BinaryOp::Shr, &int(-128, IntTy::I8), &int(7, IntTy::U8)),
            Ok(int(-1, IntTy::I8))
        );
        assert_eq!(
            binary(BinaryOp::Shr, &int(128, IntTy::U8), &int(7, IntTy::U8)),
            Ok(int(1, IntTy::U8))
        );
        assert_eq!(
            binary(BinaryOp::Shl, &int(1, IntTy::U8), &int(8, IntTy::U8)),
            Err(PanicKind::ShiftOverflow)
        );
        assert_eq!(
            unary(UnaryOp::BitNot, &int(0, IntTy::I8)),
            Ok(int(-1, IntTy::I8))
        );
        assert_eq!(
            unary(UnaryOp::BitNot, &int(0, IntTy::U16)),
            Ok(int(65535, IntTy::U16))
        );
        assert_eq!(
            unary(UnaryOp::Neg, &int(-128, IntTy::I8)),
            Err(PanicKind::Overflow)
        );
    }

    #[test]
    fn casts() {
        assert_eq!(
            cast(CastKind::As, &int(300, IntTy::I64), Ty::Int(IntTy::U8)),
            Err(PanicKind::LossyCast)
        );
        assert_eq!(
            cast(CastKind::As, &int(-1, IntTy::I64), Ty::Int(IntTy::U64)),
            Err(PanicKind::LossyCast)
        );
        assert_eq!(
            cast(
                CastKind::Reinterpret,
                &int(-1, IntTy::I8),
                Ty::Int(IntTy::U8)
            ),
            Ok(int(255, IntTy::U8))
        );
        let big = Value::Float {
            value: 1e10,
            ty: FloatTy::F64,
        };
        assert_eq!(
            cast(CastKind::As, &big, Ty::Int(IntTy::I32)),
            Ok(int(i128::from(i32::MAX), IntTy::I32))
        );
        let nan = Value::Float {
            value: f64::NAN,
            ty: FloatTy::F64,
        };
        assert_eq!(
            cast(CastKind::As, &nan, Ty::Int(IntTy::U8)),
            Ok(int(0, IntTy::U8))
        );
        assert_eq!(
            cast(CastKind::As, &Value::Char('A'), Ty::Int(IntTy::U32)),
            Ok(int(65, IntTy::U32))
        );
    }

    #[test]
    fn f32_arithmetic_rounds_to_f32() {
        let a = Value::Float {
            value: f64::from(0.1f32),
            ty: FloatTy::F32,
        };
        let sum = binary(BinaryOp::Add, &a, &a).expect("no overflow");
        assert_eq!(
            sum,
            Value::Float {
                value: f64::from(0.1f32 + 0.1f32),
                ty: FloatTy::F32
            }
        );
    }
}
