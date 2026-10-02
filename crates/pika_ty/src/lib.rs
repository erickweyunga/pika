//! The types of Pika programs, shared by every phase of the compiler.
//!
//! [`Ty`] is a small `Copy` value. Types that refer to other types, such as user-defined
//! structs, refer to interned data: interning gives every distinct type one `'static`
//! address, so types stay cheap to copy and compare.

use std::collections::HashSet;
use std::fmt;
use std::sync::{LazyLock, Mutex, PoisonError};

/// An integer type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs, reason = "the variants are the type names")]
pub enum IntTy {
    I8,
    I16,
    I32,
    I64,
    U8,
    U16,
    U32,
    U64,
}

impl IntTy {
    /// Returns true for signed types.
    pub fn is_signed(self) -> bool {
        matches!(self, Self::I8 | Self::I16 | Self::I32 | Self::I64)
    }

    /// The number of bits.
    pub fn bits(self) -> u32 {
        match self {
            Self::I8 | Self::U8 => 8,
            Self::I16 | Self::U16 => 16,
            Self::I32 | Self::U32 => 32,
            Self::I64 | Self::U64 => 64,
        }
    }

    /// The largest magnitude a literal of this type may have, for a positive (`negative ==
    /// false`) or negative literal.
    pub fn max_magnitude(self, negative: bool) -> u64 {
        let bits = self.bits();
        if self.is_signed() {
            let max_positive = (1u64 << (bits - 1)) - 1;
            if negative {
                max_positive + 1
            } else {
                max_positive
            }
        } else if negative {
            0
        } else if bits == 64 {
            u64::MAX
        } else {
            (1u64 << bits) - 1
        }
    }

    /// The type's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::I8 => "i8",
            Self::I16 => "i16",
            Self::I32 => "i32",
            Self::I64 => "i64",
            Self::U8 => "u8",
            Self::U16 => "u16",
            Self::U32 => "u32",
            Self::U64 => "u64",
        }
    }
}

/// A floating-point type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[allow(missing_docs, reason = "the variants are the type names")]
pub enum FloatTy {
    F32,
    F64,
}

impl FloatTy {
    /// The type's name.
    pub fn name(self) -> &'static str {
        match self {
            Self::F32 => "f32",
            Self::F64 => "f64",
        }
    }
}

/// A type parameter of a generic declaration: its position among the parameters in scope
/// (those of the enclosing type, then those of the function) and its name.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Param {
    /// The position: `Ty::subst` replaces it with the type at this index.
    pub index: u32,
    /// The name, for messages.
    pub name: Box<str>,
}

static PARAMS: LazyLock<Mutex<HashSet<&'static Param>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

impl Param {
    /// The interned type parameter with this position and name.
    pub fn intern(index: u32, name: &str) -> &'static Self {
        let key = Self {
            index,
            name: name.into(),
        };
        let mut params = PARAMS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(param) = params.get(&key) {
            return param;
        }
        let param: &'static Self = Box::leak(Box::new(key));
        params.insert(param);
        param
    }
}

/// A type variable, created by type inference for a type not known yet.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct TyVar(pub u32);

/// What kind of user-defined type an [`Adt`] is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum AdtKind {
    /// A `:struct`.
    Struct,
    /// An `:enum`.
    Enum,
}

/// A user-defined type ("algebraic data type"): its kind, the index of its declaration in
/// the module, its name, and the types given for its type parameters.
#[derive(Debug, PartialEq, Eq, Hash)]
pub struct Adt {
    /// Whether it is a struct or an enum.
    pub kind: AdtKind,
    /// The index of its declaration in the module.
    pub index: u32,
    /// Its name.
    pub name: Box<str>,
    /// The types of its type parameters, in order (none for a type without parameters).
    pub args: Box<[Ty]>,
}

static ADTS: LazyLock<Mutex<HashSet<&'static Adt>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

impl Adt {
    /// The interned user-defined type with these properties.
    pub fn intern(kind: AdtKind, index: u32, name: &str, args: &[Ty]) -> &'static Self {
        let key = Self {
            kind,
            index,
            name: name.into(),
            args: args.into(),
        };
        let mut adts = ADTS.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(adt) = adts.get(&key) {
            return adt;
        }
        let adt: &'static Self = Box::leak(Box::new(key));
        adts.insert(adt);
        adt
    }
}

/// A type.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Ty {
    /// An integer type.
    Int(IntTy),
    /// A floating-point type.
    Float(FloatTy),
    /// `bool`
    Bool,
    /// `char`
    Char,
    /// `String`
    String,
    /// `Duration`
    Duration,
    /// `Formatter`: the text being written by a `fmt` function (spec section 10.4).
    Formatter,
    /// `nothing`
    Nothing,
    /// `never`: the type of expressions that do not finish.
    Never,
    /// A user-defined type.
    Adt(&'static Adt),
    /// A type parameter, inside a generic declaration.
    Param(&'static Param),
    /// `T?`: `some` value or `none`.
    Option(&'static Ty),
    /// `Box<T>`: a value on the heap.
    Box(&'static Ty),
    /// `List<T>`
    List(&'static Ty),
    /// `Map<K, V>`
    Map(&'static MapTy),
    /// `Set<T>`
    Set(&'static Ty),
    /// `fn(A, B) -> R raises`: a function value.
    Fn(&'static FnTy),
    /// A type not known yet (only during type inference).
    Var(TyVar),
    /// An erroneous type, after a reported error. Compatible with everything.
    Error,
}

impl Ty {
    /// The built-in type with the given name, including the aliases `int` and `float`.
    pub fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "i8" => Self::Int(IntTy::I8),
            "i16" => Self::Int(IntTy::I16),
            "i32" => Self::Int(IntTy::I32),
            "i64" | "int" => Self::Int(IntTy::I64),
            "u8" => Self::Int(IntTy::U8),
            "u16" => Self::Int(IntTy::U16),
            "u32" => Self::Int(IntTy::U32),
            "u64" => Self::Int(IntTy::U64),
            "f32" => Self::Float(FloatTy::F32),
            "f64" | "float" => Self::Float(FloatTy::F64),
            "bool" => Self::Bool,
            "char" => Self::Char,
            "String" => Self::String,
            "Duration" => Self::Duration,
            "Formatter" => Self::Formatter,
            "nothing" => Self::Nothing,
            "never" => Self::Never,
            _ => return None,
        })
    }

    /// The struct this type is, if it is one.
    pub fn as_struct(self) -> Option<&'static Adt> {
        match self {
            Self::Adt(adt) if adt.kind == AdtKind::Struct => Some(adt),
            _ => None,
        }
    }

    /// The enum this type is, if it is one.
    pub fn as_enum(self) -> Option<&'static Adt> {
        match self {
            Self::Adt(adt) if adt.kind == AdtKind::Enum => Some(adt),
            _ => None,
        }
    }

    /// `inner?`
    pub fn option(inner: Self) -> Self {
        Self::Option(intern(inner))
    }

    /// `Box<inner>`
    pub fn boxed(inner: Self) -> Self {
        Self::Box(intern(inner))
    }

    /// `List<element>`
    pub fn list(element: Self) -> Self {
        Self::List(intern(element))
    }

    /// `Set<element>`
    pub fn set(element: Self) -> Self {
        Self::Set(intern(element))
    }

    /// `fn(params) -> ret`, with `raises` if `raises`.
    pub fn function(params: &[Self], ret: Self, raises: bool) -> Self {
        Self::Fn(intern_fn(FnTy {
            params: params.into(),
            ret,
            raises,
        }))
    }

    /// `Map<key, value>`
    pub fn map_of(key: Self, value: Self) -> Self {
        Self::Map(intern_map(MapTy { key, value }))
    }

    /// The types this type is built from: the value of an option or box, the elements of a
    /// list or set, the key and value of a map.
    pub fn components(self) -> Vec<Self> {
        match self {
            Self::Option(inner) | Self::Box(inner) | Self::List(inner) | Self::Set(inner) => {
                vec![*inner]
            }
            Self::Map(map) => vec![map.key, map.value],
            Self::Fn(function) => function
                .params
                .iter()
                .copied()
                .chain(std::iter::once(function.ret))
                .collect(),
            Self::Adt(adt) => adt.args.to_vec(),
            _ => Vec::new(),
        }
    }

    /// Returns true if the type contains a type parameter.
    pub fn has_params(self) -> bool {
        match self {
            Self::Param(_) => true,
            other => other.components().into_iter().any(Self::has_params),
        }
    }

    /// The type with each parameter replaced by the type at its index in `args`.
    #[must_use]
    pub fn subst(self, args: &[Self]) -> Self {
        if args.is_empty() || !self.has_params() {
            return self;
        }
        self.map(&mut |t| match t {
            Self::Param(param) => args.get(param.index as usize).copied().unwrap_or(t),
            other => other,
        })
    }

    /// A user-defined type with the given type arguments.
    pub fn adt(adt: &'static Adt, args: &[Self]) -> Self {
        Self::Adt(Adt::intern(adt.kind, adt.index, &adt.name, args))
    }

    /// Returns true if the type contains a type variable.
    pub fn has_vars(self) -> bool {
        match self {
            Self::Var(_) => true,
            other => other.components().into_iter().any(Self::has_vars),
        }
    }

    /// The type with every component replaced by `f` of it, innermost first.
    #[must_use]
    pub fn map(self, f: &mut impl FnMut(Self) -> Self) -> Self {
        let mapped = match self {
            Self::Option(inner) => Self::option(inner.map(f)),
            Self::Box(inner) => Self::boxed(inner.map(f)),
            Self::List(inner) => Self::list(inner.map(f)),
            Self::Set(inner) => Self::set(inner.map(f)),
            Self::Map(map) => Self::map_of(map.key.map(f), map.value.map(f)),
            Self::Fn(function) => {
                let params: Vec<Self> = function.params.iter().map(|p| p.map(f)).collect();
                Self::function(&params, function.ret.map(f), function.raises)
            }
            Self::Adt(adt) if !adt.args.is_empty() => {
                let args: Vec<Self> = adt.args.iter().map(|a| a.map(f)).collect();
                Self::adt(adt, &args)
            }
            other => other,
        };
        f(mapped)
    }
}

/// The key and value types of a `Map`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct MapTy {
    /// The type of the keys.
    pub key: Ty,
    /// The type of the values.
    pub value: Ty,
}

/// The signature of a function value.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct FnTy {
    /// The types of the parameters, which are borrowed for reading.
    pub params: Box<[Ty]>,
    /// The type of the result.
    pub ret: Ty,
    /// Whether calls may raise an error.
    pub raises: bool,
}

static FNS: LazyLock<Mutex<HashSet<&'static FnTy>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

fn intern_fn(function: FnTy) -> &'static FnTy {
    let mut fns = FNS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(interned) = fns.get(&function) {
        return interned;
    }
    let interned: &'static FnTy = Box::leak(Box::new(function));
    fns.insert(interned);
    interned
}

static MAPS: LazyLock<Mutex<HashSet<&'static MapTy>>> =
    LazyLock::new(|| Mutex::new(HashSet::new()));

fn intern_map(map: MapTy) -> &'static MapTy {
    let mut maps = MAPS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(interned) = maps.get(&map) {
        return interned;
    }
    let interned: &'static MapTy = Box::leak(Box::new(map));
    maps.insert(interned);
    interned
}

static TYS: LazyLock<Mutex<HashSet<&'static Ty>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// The interned copy of a type, for types that contain other types.
fn intern(ty: Ty) -> &'static Ty {
    let mut tys = TYS.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(interned) = tys.get(&ty) {
        return interned;
    }
    let interned: &'static Ty = Box::leak(Box::new(ty));
    tys.insert(interned);
    interned
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Int(int) => f.write_str(int.name()),
            Self::Float(float) => f.write_str(float.name()),
            Self::Bool => f.write_str("bool"),
            Self::Char => f.write_str("char"),
            Self::String => f.write_str("String"),
            Self::Duration => f.write_str("Duration"),
            Self::Formatter => f.write_str("Formatter"),
            Self::Nothing => f.write_str("nothing"),
            Self::Never => f.write_str("never"),
            Self::Adt(adt) if adt.args.is_empty() => f.write_str(&adt.name),
            Self::Adt(adt) => {
                let args: Vec<String> = adt.args.iter().map(ToString::to_string).collect();
                write!(f, "{}<{}>", adt.name, args.join(", "))
            }
            Self::Param(param) => f.write_str(&param.name),
            Self::Option(inner) => write!(f, "{inner}?"),
            Self::Box(inner) => write!(f, "Box<{inner}>"),
            Self::List(inner) => write!(f, "List<{inner}>"),
            Self::Set(inner) => write!(f, "Set<{inner}>"),
            Self::Map(map) => write!(f, "Map<{}, {}>", map.key, map.value),
            Self::Fn(function) => {
                let params: Vec<String> = function.params.iter().map(ToString::to_string).collect();
                write!(f, "fn({})", params.join(", "))?;
                if function.ret != Self::Nothing {
                    write!(f, " -> {}", function.ret)?;
                }
                if function.raises {
                    f.write_str(" raises")?;
                }
                Ok(())
            }
            Self::Var(_) => f.write_str("_"),
            Self::Error => f.write_str("{error}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn literal_ranges() {
        assert_eq!(IntTy::I8.max_magnitude(false), 127);
        assert_eq!(IntTy::I8.max_magnitude(true), 128);
        assert_eq!(IntTy::U8.max_magnitude(false), 255);
        assert_eq!(IntTy::U8.max_magnitude(true), 0);
        assert_eq!(IntTy::I64.max_magnitude(false), i64::MAX as u64);
        assert_eq!(IntTy::I64.max_magnitude(true), i64::MIN.unsigned_abs());
        assert_eq!(IntTy::U64.max_magnitude(false), u64::MAX);
    }
}

#[cfg(test)]
mod interning {
    use super::*;

    #[test]
    fn adts_are_interned_by_value() {
        let a = Adt::intern(AdtKind::Struct, 0, "Point", &[]);
        let b = Adt::intern(AdtKind::Struct, 0, "Point", &[]);
        assert!(std::ptr::eq(a, b));
        assert_ne!(
            Ty::Adt(a),
            Ty::Adt(Adt::intern(AdtKind::Struct, 1, "Point", &[]))
        );
        assert_eq!(Ty::Adt(a).to_string(), "Point");
    }

    #[test]
    fn generic_types_substitute_their_parameters() {
        let (t, u) = (Param::intern(0, "T"), Param::intern(1, "U"));
        let pair = Adt::intern(AdtKind::Struct, 2, "Pair", &[Ty::Param(t), Ty::Param(u)]);
        let generic = Ty::list(Ty::Adt(pair));
        assert!(generic.has_params());
        assert_eq!(generic.to_string(), "List<Pair<T, U>>");
        let concrete = generic.subst(&[Ty::Int(IntTy::I64), Ty::option(Ty::Param(t))]);
        assert_eq!(concrete.to_string(), "List<Pair<i64, T?>>");
        assert_eq!(
            concrete.subst(&[Ty::Bool]).to_string(),
            "List<Pair<i64, bool?>>"
        );
    }

    #[test]
    fn compound_types_are_interned_by_value() {
        let a = Ty::option(Ty::boxed(Ty::String));
        let b = Ty::option(Ty::boxed(Ty::String));
        assert_eq!(a, b);
        assert_ne!(a, Ty::option(Ty::String));
        assert_eq!(a.to_string(), "Box<String>?");
        assert!(Ty::option(Ty::Var(TyVar(0))).has_vars());
        let resolved = Ty::option(Ty::Var(TyVar(0))).map(&mut |t| match t {
            Ty::Var(_) => Ty::Bool,
            other => other,
        });
        assert_eq!(resolved, Ty::option(Ty::Bool));
        let map = Ty::map_of(Ty::String, Ty::list(Ty::Var(TyVar(1))));
        assert_eq!(map, Ty::map_of(Ty::String, Ty::list(Ty::Var(TyVar(1)))));
        assert!(map.has_vars());
        assert_eq!(map.to_string(), "Map<String, List<_>>");
    }
}
