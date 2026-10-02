//! Intrinsics: the operations of the runtime that the standard library declares with
//! `:extern lib="pika" { ... }`, for what Pika code cannot do itself, such as float math,
//! text processing and input and output.
//!
//! Each intrinsic is implemented once, by [`Intrinsic::call`], which both the compiler's
//! interpreter and compiled code use (compiled code through
//! [`pika_intrinsic`](crate::abi::pika_intrinsic)), so that they behave exactly alike.

use std::io::BufRead;
use std::sync::{LazyLock, Mutex, PoisonError};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// The kind of a parameter or result of an intrinsic, and so its Pika type.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `i64`.
    Int,
    /// `f64`.
    Float,
    /// `bool`.
    Bool,
    /// `char`.
    Char,
    /// `String`, read without taking ownership as a parameter.
    Str,
    /// `Duration`, in nanoseconds.
    Duration,
    /// `nothing`, as a result.
    Nothing,
}

/// The value of an argument of an intrinsic.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Arg<'a> {
    /// An `i64`.
    Int(i64),
    /// An `f64`.
    Float(f64),
    /// A `bool`.
    Bool(bool),
    /// A `char`.
    Char(char),
    /// A `String`.
    Str(&'a str),
    /// A `Duration`, in nanoseconds.
    Duration(i64),
}

/// The result of an intrinsic.
#[derive(Clone, Debug, PartialEq)]
pub enum Ret {
    /// An `i64`.
    Int(i64),
    /// An `f64`.
    Float(f64),
    /// A `bool`.
    Bool(bool),
    /// A `char`.
    Char(char),
    /// A new `String`.
    Str(String),
    /// A `Duration`, in nanoseconds.
    Duration(i64),
    /// `nothing`.
    Nothing,
}

/// Where the input and output intrinsics read and write: the process's streams for compiled
/// code, and the interpreter's own for interpreted code.
pub trait Io {
    /// Writes text to standard output.
    fn write_out(&mut self, text: &str);
    /// Writes text to standard error.
    fn write_err(&mut self, text: &str);
    /// Flushes standard output, then reads a line from standard input, with its line break;
    /// empty at the end of the input.
    fn read_line(&mut self) -> String;
}

/// Reads a line from the process's standard input, as [`Io::read_line`] does after flushing.
pub fn read_stdin_line() -> String {
    let mut line = String::new();
    // A read error ends the input, like the end of the file.
    if std::io::stdin().lock().read_line(&mut line).is_err() {
        line.clear();
    }
    line
}

macro_rules! intrinsics {
    ($($variant:ident $name:literal $pure:literal ($($param:ident),*) -> $ret:ident;)*) => {
        /// An intrinsic. The discriminants are part of the interface between compiled code
        /// and the runtime.
        #[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
        #[repr(u32)]
        #[allow(missing_docs, reason = "each intrinsic is documented by its name")]
        pub enum Intrinsic {
            $($variant,)*
        }

        impl Intrinsic {
            /// Every intrinsic, in order of discriminant.
            pub const ALL: &[Self] = &[$(Self::$variant,)*];

            /// The name the standard library declares it with.
            pub fn name(self) -> &'static str {
                match self {
                    $(Self::$variant => $name,)*
                }
            }

            /// The kinds of its parameters.
            pub fn params(self) -> &'static [Kind] {
                match self {
                    $(Self::$variant => &[$(Kind::$param),*],)*
                }
            }

            /// The kind of its result.
            pub fn ret(self) -> Kind {
                match self {
                    $(Self::$variant => Kind::$ret,)*
                }
            }

            /// Whether it only computes its result from its arguments, without input,
            /// output or other effects, so that constants can use it.
            pub fn is_pure(self) -> bool {
                match self {
                    $(Self::$variant => $pure,)*
                }
            }
        }
    };
}

intrinsics! {
    Sqrt "sqrt" true (Float) -> Float;
    Cbrt "cbrt" true (Float) -> Float;
    Exp "exp" true (Float) -> Float;
    Ln "ln" true (Float) -> Float;
    Log2 "log2" true (Float) -> Float;
    Log10 "log10" true (Float) -> Float;
    Sin "sin" true (Float) -> Float;
    Cos "cos" true (Float) -> Float;
    Tan "tan" true (Float) -> Float;
    Asin "asin" true (Float) -> Float;
    Acos "acos" true (Float) -> Float;
    Atan "atan" true (Float) -> Float;
    Floor "floor" true (Float) -> Float;
    Ceil "ceil" true (Float) -> Float;
    Round "round" true (Float) -> Float;
    Trunc "trunc" true (Float) -> Float;
    Pow "pow" true (Float, Float) -> Float;
    Atan2 "atan2" true (Float, Float) -> Float;
    Hypot "hypot" true (Float, Float) -> Float;
    Sinh "sinh" true (Float) -> Float;
    Cosh "cosh" true (Float) -> Float;
    Tanh "tanh" true (Float) -> Float;
    Asinh "asinh" true (Float) -> Float;
    Acosh "acosh" true (Float) -> Float;
    Atanh "atanh" true (Float) -> Float;
    Exp2 "exp2" true (Float) -> Float;
    ExpM1 "exp_m1" true (Float) -> Float;
    Ln1p "ln_1p" true (Float) -> Float;
    RoundEven "round_even" true (Float) -> Float;
    Copysign "copysign" true (Float, Float) -> Float;
    MulAdd "mul_add" true (Float, Float, Float) -> Float;
    NextUp "next_up" true (Float) -> Float;
    NextDown "next_down" true (Float) -> Float;
    Gamma "gamma" true (Float) -> Float;
    LnGamma "ln_gamma" true (Float) -> Float;
    Erf "erf" true (Float) -> Float;
    Erfc "erfc" true (Float) -> Float;
    FloatToBits "float_to_bits" true (Float) -> Int;
    FloatFromBits "float_from_bits" true (Int) -> Float;
    IntWrappingAdd "int_wrapping_add" true (Int, Int) -> Int;
    IntWrappingSub "int_wrapping_sub" true (Int, Int) -> Int;
    IntWrappingMul "int_wrapping_mul" true (Int, Int) -> Int;
    IntAddOverflows "int_add_overflows" true (Int, Int) -> Bool;
    IntSubOverflows "int_sub_overflows" true (Int, Int) -> Bool;
    IntMulOverflows "int_mul_overflows" true (Int, Int) -> Bool;
    IntCountOnes "int_count_ones" true (Int) -> Int;
    IntLeadingZeros "int_leading_zeros" true (Int) -> Int;
    IntTrailingZeros "int_trailing_zeros" true (Int) -> Int;
    IntRotateLeft "int_rotate_left" true (Int, Int) -> Int;
    IntReverseBits "int_reverse_bits" true (Int) -> Int;
    IntSwapBytes "int_swap_bytes" true (Int) -> Int;
    IntToStringBase "int_to_string_base" true (Int, Int) -> Str;
    FloatToStringFixed "float_to_string_fixed" true (Float, Int) -> Str;
    FloatToStringExp "float_to_string_exp" true (Float, Int) -> Str;
    StrFind "str_find" true (Str, Str, Int) -> Int;
    StrSlice "str_slice" true (Str, Int, Int) -> Str;
    StrIsBoundary "str_is_boundary" true (Str, Int) -> Bool;
    StrCharAt "str_char_at" true (Str, Int) -> Char;
    StrByteAt "str_byte_at" true (Str, Int) -> Int;
    StrToUpper "str_to_upper" true (Str) -> Str;
    StrToLower "str_to_lower" true (Str) -> Str;
    StrIsInt "str_is_int" true (Str) -> Bool;
    StrToInt "str_to_int" true (Str) -> Int;
    StrIsFloat "str_is_float" true (Str) -> Bool;
    StrToFloat "str_to_float" true (Str) -> Float;
    StrRfind "str_rfind" true (Str, Str) -> Int;
    StrIsIntBase "str_is_int_base" true (Str, Int) -> Bool;
    StrToIntBase "str_to_int_base" true (Str, Int) -> Int;
    CharIsWhitespace "char_is_whitespace" true (Char) -> Bool;
    CharIsAlphabetic "char_is_alphabetic" true (Char) -> Bool;
    CharIsNumeric "char_is_numeric" true (Char) -> Bool;
    CharIsAlphanumeric "char_is_alphanumeric" true (Char) -> Bool;
    CharIsUppercase "char_is_uppercase" true (Char) -> Bool;
    CharIsLowercase "char_is_lowercase" true (Char) -> Bool;
    CharToUpper "char_to_upper" true (Char) -> Char;
    CharToLower "char_to_lower" true (Char) -> Char;
    CharIsValid "char_is_valid" true (Int) -> Bool;
    CharFromInt "char_from_int" true (Int) -> Char;
    Print "print" false (Str) -> Nothing;
    EPrint "eprint" false (Str) -> Nothing;
    ReadLine "read_line" false () -> Str;
    ArgCount "arg_count" false () -> Int;
    Arg "arg" false (Int) -> Str;
    EnvHas "env_has" false (Str) -> Bool;
    EnvGet "env_get" false (Str) -> Str;
    Monotonic "monotonic" false () -> Duration;
    UnixTime "unix_time" false () -> Duration;
    Sleep "sleep" false (Duration) -> Nothing;
    FsRead "fs_read" false (Str) -> Str;
    FsWrite "fs_write" false (Str, Str) -> Nothing;
    FsExists "fs_exists" false (Str) -> Bool;
    FsRemove "fs_remove" false (Str) -> Nothing;
    FsError "fs_error" false () -> Str;
    RandomMix "random_mix" true (Int) -> Int;
    RandomBelow "random_below" true (Int, Int) -> Int;
    RandomUnit "random_unit" true (Int) -> Float;
    RandomSeed "random_seed" false () -> Int;
}

/// The arguments of the running program, after the program itself.
static PROGRAM_ARGS: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Sets the arguments that `/std/env/args` gives the program.
pub fn set_program_args(args: Vec<String>) {
    *PROGRAM_ARGS.lock().unwrap_or_else(PoisonError::into_inner) = args;
}

/// When the program started, for [`Intrinsic::Monotonic`].
static START: LazyLock<Instant> = LazyLock::new(Instant::now);

/// Starts the clock of [`Intrinsic::Monotonic`]; called when a program starts.
pub fn start_clock() {
    LazyLock::force(&START);
}

/// The message of the last failed file system operation, empty after a successful one.
static FS_ERROR: Mutex<String> = Mutex::new(String::new());

/// Records the outcome of a file system operation.
fn fs_outcome<T>(result: std::io::Result<T>) -> Option<T> {
    let mut error = FS_ERROR.lock().unwrap_or_else(PoisonError::into_inner);
    match result {
        Ok(value) => {
            error.clear();
            Some(value)
        }
        Err(failure) => {
            *error = failure.to_string();
            None
        }
    }
}

/// Nanoseconds in a `Duration`, saturated to the range of `i64`.
fn nanos(duration: Duration) -> i64 {
    i64::try_from(duration.as_nanos()).unwrap_or(i64::MAX)
}

/// A byte offset of a string from an `i64`, if it is within the string.
fn offset(text: &str, index: i64) -> Option<usize> {
    usize::try_from(index).ok().filter(|&i| i <= text.len())
}

/// The only character of an iterator, or `fallback` if it has none or several: a case
/// mapping that changes the number of characters keeps the character.
fn single(mut chars: impl Iterator<Item = char>, fallback: char) -> char {
    match (chars.next(), chars.next()) {
        (Some(c), None) => c,
        _ => fallback,
    }
}

impl Intrinsic {
    /// The intrinsic with discriminant `code`.
    pub fn from_code(code: u32) -> Option<Self> {
        Self::ALL.get(code as usize).copied()
    }

    /// The intrinsic named `name`.
    pub fn from_name(name: &str) -> Option<Self> {
        Self::ALL.iter().copied().find(|i| i.name() == name)
    }

    /// Runs the intrinsic on `args`, which match its parameters, reading and writing `io`.
    ///
    /// # Panics
    ///
    /// Panics if the arguments do not match the parameters, which the compiler ensures.
    pub fn call(self, args: &[Arg<'_>], io: &mut dyn Io) -> Ret {
        if let Some(result) = self.call_math(args) {
            return Ret::Float(result);
        }
        if let Some(result) = self.call_text(args) {
            return result;
        }
        if let Some(result) = self.call_numbers(args) {
            return result;
        }
        self.call_system(args, io)
    }

    /// The float math intrinsics.
    fn call_math(self, args: &[Arg<'_>]) -> Option<f64> {
        let x = || float(args, 0);
        let y = || float(args, 1);
        Some(match self {
            Self::Sqrt => x().sqrt(),
            Self::Cbrt => x().cbrt(),
            Self::Exp => x().exp(),
            Self::Ln => x().ln(),
            Self::Log2 => x().log2(),
            Self::Log10 => x().log10(),
            Self::Sin => x().sin(),
            Self::Cos => x().cos(),
            Self::Tan => x().tan(),
            Self::Asin => x().asin(),
            Self::Acos => x().acos(),
            Self::Atan => x().atan(),
            Self::Floor => x().floor(),
            Self::Ceil => x().ceil(),
            Self::Round => x().round(),
            Self::Trunc => x().trunc(),
            Self::Pow => x().powf(y()),
            Self::Atan2 => x().atan2(y()),
            Self::Hypot => x().hypot(y()),
            Self::Sinh => x().sinh(),
            Self::Cosh => x().cosh(),
            Self::Tanh => x().tanh(),
            Self::Asinh => x().asinh(),
            Self::Acosh => x().acosh(),
            Self::Atanh => x().atanh(),
            Self::Exp2 => x().exp2(),
            Self::ExpM1 => x().exp_m1(),
            Self::Ln1p => x().ln_1p(),
            Self::RoundEven => x().round_ties_even(),
            Self::Copysign => x().copysign(y()),
            Self::MulAdd => x().mul_add(y(), float(args, 2)),
            Self::NextUp => x().next_up(),
            Self::NextDown => x().next_down(),
            Self::Gamma => libm::tgamma(x()),
            Self::LnGamma => libm::lgamma(x()),
            Self::Erf => libm::erf(x()),
            Self::Erfc => libm::erfc(x()),
            Self::FloatFromBits => f64::from_bits(int(args, 0).cast_unsigned()),
            _ => return None,
        })
    }

    /// The intrinsics on strings and characters.
    fn call_text(self, args: &[Arg<'_>]) -> Option<Ret> {
        let s = || string(args, 0);
        let c = || character(args, 0);
        Some(match self {
            Self::StrFind => {
                let (text, needle) = (s(), string(args, 1));
                let found = offset(text, int(args, 2))
                    .filter(|&from| text.is_char_boundary(from))
                    .and_then(|from| text[from..].find(needle).map(|i| i + from));
                Ret::Int(found.map_or(-1, |i| i64::try_from(i).unwrap_or(-1)))
            }
            Self::StrSlice => {
                let text = s();
                let slice = match (offset(text, int(args, 1)), offset(text, int(args, 2))) {
                    (Some(start), Some(end)) if start <= end => text.get(start..end),
                    _ => None,
                };
                Ret::Str(slice.unwrap_or_default().to_owned())
            }
            Self::StrIsBoundary => {
                Ret::Bool(offset(s(), int(args, 1)).is_some_and(|i| s().is_char_boundary(i)))
            }
            Self::StrCharAt => {
                let text = s();
                let found = offset(text, int(args, 1))
                    .and_then(|i| text.get(i..))
                    .and_then(|rest| rest.chars().next());
                Ret::Char(found.unwrap_or('\u{FFFD}'))
            }
            Self::StrByteAt => {
                let byte = usize::try_from(int(args, 1))
                    .ok()
                    .and_then(|i| s().as_bytes().get(i).copied());
                Ret::Int(byte.map_or(-1, i64::from))
            }
            Self::StrToUpper => Ret::Str(s().to_uppercase()),
            Self::StrToLower => Ret::Str(s().to_lowercase()),
            Self::StrIsInt => Ret::Bool(s().parse::<i64>().is_ok()),
            Self::StrToInt => Ret::Int(s().parse().unwrap_or(0)),
            Self::StrIsFloat => Ret::Bool(s().parse::<f64>().is_ok()),
            Self::StrToFloat => Ret::Float(s().parse().unwrap_or(0.0)),
            Self::StrRfind => Ret::Int(
                s().rfind(string(args, 1))
                    .map_or(-1, |i| i64::try_from(i).unwrap_or(-1)),
            ),
            Self::StrIsIntBase => Ret::Bool(parse_base(s(), int(args, 1)).is_some()),
            Self::StrToIntBase => Ret::Int(parse_base(s(), int(args, 1)).unwrap_or(0)),
            Self::IntToStringBase => Ret::Str(format_base(int(args, 0), int(args, 1))),
            Self::FloatToStringFixed => {
                Ret::Str(format!("{:.*}", precision(args, 1), float(args, 0)))
            }
            Self::FloatToStringExp => {
                Ret::Str(format!("{:.*e}", precision(args, 1), float(args, 0)))
            }
            Self::CharIsWhitespace => Ret::Bool(c().is_whitespace()),
            Self::CharIsAlphabetic => Ret::Bool(c().is_alphabetic()),
            Self::CharIsNumeric => Ret::Bool(c().is_numeric()),
            Self::CharIsAlphanumeric => Ret::Bool(c().is_alphanumeric()),
            Self::CharIsUppercase => Ret::Bool(c().is_uppercase()),
            Self::CharIsLowercase => Ret::Bool(c().is_lowercase()),
            Self::CharToUpper => Ret::Char(single(c().to_uppercase(), c())),
            Self::CharToLower => Ret::Char(single(c().to_lowercase(), c())),
            Self::CharIsValid => Ret::Bool(
                u32::try_from(int(args, 0))
                    .ok()
                    .and_then(char::from_u32)
                    .is_some(),
            ),
            Self::CharFromInt => Ret::Char(
                u32::try_from(int(args, 0))
                    .ok()
                    .and_then(char::from_u32)
                    .unwrap_or('\u{FFFD}'),
            ),
            _ => return None,
        })
    }

    /// The intrinsics on integers and of random numbers.
    fn call_numbers(self, args: &[Arg<'_>]) -> Option<Ret> {
        let a = || int(args, 0);
        let b = || int(args, 1);
        let count = |n: u32| Ret::Int(i64::from(n));
        Some(match self {
            Self::FloatToBits => Ret::Int(float(args, 0).to_bits().cast_signed()),
            Self::IntWrappingAdd => Ret::Int(a().wrapping_add(b())),
            Self::IntWrappingSub => Ret::Int(a().wrapping_sub(b())),
            Self::IntWrappingMul => Ret::Int(a().wrapping_mul(b())),
            Self::IntAddOverflows => Ret::Bool(a().checked_add(b()).is_none()),
            Self::IntSubOverflows => Ret::Bool(a().checked_sub(b()).is_none()),
            Self::IntMulOverflows => Ret::Bool(a().checked_mul(b()).is_none()),
            Self::IntCountOnes => count(a().count_ones()),
            Self::IntLeadingZeros => count(a().leading_zeros()),
            Self::IntTrailingZeros => count(a().trailing_zeros()),
            Self::IntRotateLeft => {
                let by = u32::try_from(b().rem_euclid(64)).expect("below 64");
                Ret::Int(a().rotate_left(by))
            }
            Self::IntReverseBits => Ret::Int(a().reverse_bits()),
            Self::IntSwapBytes => Ret::Int(a().swap_bytes()),
            Self::RandomMix => Ret::Int(splitmix64(a().cast_unsigned()).cast_signed()),
            Self::RandomBelow => {
                // Rejects the values at or above the largest multiple of the bound, so that
                // every result is equally likely.
                let (bits, bound) = (a().cast_unsigned(), b().cast_unsigned());
                let zone = u64::MAX - (u64::MAX % bound.max(1));
                Ret::Int(if bound == 0 || bits >= zone {
                    -1
                } else {
                    (bits % bound).cast_signed()
                })
            }
            Self::RandomUnit => Ret::Float(unit_float(a().cast_unsigned())),
            Self::RandomSeed => Ret::Int(entropy().cast_signed()),
            _ => return None,
        })
    }

    /// The intrinsics of input, output, the environment, time and files.
    fn call_system(self, args: &[Arg<'_>], io: &mut dyn Io) -> Ret {
        match self {
            Self::Print => {
                io.write_out(string(args, 0));
                Ret::Nothing
            }
            Self::EPrint => {
                io.write_err(string(args, 0));
                Ret::Nothing
            }
            Self::ReadLine => Ret::Str(io.read_line()),
            Self::ArgCount => {
                let count = PROGRAM_ARGS
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .len();
                Ret::Int(i64::try_from(count).unwrap_or(i64::MAX))
            }
            Self::Arg => {
                let args_list = PROGRAM_ARGS.lock().unwrap_or_else(PoisonError::into_inner);
                let found = usize::try_from(int(args, 0))
                    .ok()
                    .and_then(|i| args_list.get(i));
                Ret::Str(found.cloned().unwrap_or_default())
            }
            Self::EnvHas => Ret::Bool(std::env::var(string(args, 0)).is_ok()),
            Self::EnvGet => Ret::Str(std::env::var(string(args, 0)).unwrap_or_default()),
            Self::Monotonic => Ret::Duration(nanos(START.elapsed())),
            Self::UnixTime => Ret::Duration(nanos(
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default(),
            )),
            Self::Sleep => {
                let requested = duration(args, 0);
                if let Ok(nanos) = u64::try_from(requested) {
                    std::thread::sleep(Duration::from_nanos(nanos));
                }
                Ret::Nothing
            }
            Self::FsRead => {
                Ret::Str(fs_outcome(std::fs::read_to_string(string(args, 0))).unwrap_or_default())
            }
            Self::FsWrite => {
                fs_outcome(std::fs::write(string(args, 0), string(args, 1)));
                Ret::Nothing
            }
            Self::FsExists => Ret::Bool(std::path::Path::new(string(args, 0)).exists()),
            Self::FsRemove => {
                fs_outcome(std::fs::remove_file(string(args, 0)));
                Ret::Nothing
            }
            Self::FsError => Ret::Str(
                FS_ERROR
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner)
                    .clone(),
            ),
            other => unreachable!("`{}` is not a system intrinsic", other.name()),
        }
    }
}

/// The output of the `SplitMix64` generator for the state `state`: a well-mixed function of it.
fn splitmix64(state: u64) -> u64 {
    let mut z = state;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// A float from 0 (included) to 1 (excluded) made of the top 53 bits of `bits`.
fn unit_float(bits: u64) -> f64 {
    #[allow(clippy::cast_precision_loss, reason = "53 bits fit an f64 exactly")]
    let unit = (bits >> 11) as f64 / (1u64 << 53) as f64;
    unit
}

/// Unpredictable bits, from the keys that the Rust standard library seeds from the operating
/// system for hashing, and the time.
fn entropy() -> u64 {
    use std::hash::{BuildHasher, Hasher};
    let mut hasher = std::collections::hash_map::RandomState::new().build_hasher();
    hasher.write_u128(
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos(),
    );
    hasher.finish()
}

/// The integer written in `text` in base `base` (2 to 36), with an optional sign.
fn parse_base(text: &str, base: i64) -> Option<i64> {
    let base = u32::try_from(base).ok().filter(|b| (2..=36).contains(b))?;
    i64::from_str_radix(text, base).ok()
}

/// `value` written in base `base` (2 to 36), with lowercase letters for digits above 9.
fn format_base(value: i64, base: i64) -> String {
    let Some(base) = u64::try_from(base).ok().filter(|b| (2..=36).contains(b)) else {
        return String::new();
    };
    let mut magnitude = value.unsigned_abs();
    let mut digits = Vec::new();
    loop {
        let digit = u32::try_from(magnitude % base).expect("below 36");
        digits.push(char::from_digit(digit, 36).expect("a valid digit"));
        magnitude /= base;
        if magnitude == 0 {
            break;
        }
    }
    if value < 0 {
        digits.push('-');
    }
    digits.iter().rev().collect()
}

/// A number of digits after the point, from an argument, limited to what formatting allows.
fn precision(args: &[Arg<'_>], index: usize) -> usize {
    usize::try_from(int(args, index).clamp(0, 100)).expect("in range")
}

fn int(args: &[Arg<'_>], index: usize) -> i64 {
    match args[index] {
        Arg::Int(value) => value,
        other => panic!("expected an integer argument, got {other:?}"),
    }
}

fn float(args: &[Arg<'_>], index: usize) -> f64 {
    match args[index] {
        Arg::Float(value) => value,
        other => panic!("expected a float argument, got {other:?}"),
    }
}

fn character(args: &[Arg<'_>], index: usize) -> char {
    match args[index] {
        Arg::Char(value) => value,
        other => panic!("expected a char argument, got {other:?}"),
    }
}

fn string<'a>(args: &[Arg<'a>], index: usize) -> &'a str {
    match args[index] {
        Arg::Str(value) => value,
        other => panic!("expected a string argument, got {other:?}"),
    }
}

fn duration(args: &[Arg<'_>], index: usize) -> i64 {
    match args[index] {
        Arg::Duration(value) => value,
        other => panic!("expected a duration argument, got {other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct NoIo;

    impl Io for NoIo {
        fn write_out(&mut self, _: &str) {}
        fn write_err(&mut self, _: &str) {}
        fn read_line(&mut self) -> String {
            String::new()
        }
    }

    fn call(intrinsic: Intrinsic, args: &[Arg<'_>]) -> Ret {
        intrinsic.call(args, &mut NoIo)
    }

    #[test]
    fn table_is_consistent() {
        for (code, intrinsic) in Intrinsic::ALL.iter().enumerate() {
            assert_eq!(
                Intrinsic::from_code(u32::try_from(code).unwrap()),
                Some(*intrinsic)
            );
            assert_eq!(Intrinsic::from_name(intrinsic.name()), Some(*intrinsic));
            assert!(!intrinsic.params().contains(&Kind::Nothing));
        }
    }

    #[test]
    fn strings() {
        let text = Arg::Str("héllo wörld");
        assert_eq!(
            call(Intrinsic::StrFind, &[text, Arg::Str("o"), Arg::Int(0)]),
            Ret::Int(5)
        );
        assert_eq!(
            call(Intrinsic::StrFind, &[text, Arg::Str("o"), Arg::Int(6)]),
            Ret::Int(-1)
        );
        assert_eq!(
            call(Intrinsic::StrFind, &[text, Arg::Str("l"), Arg::Int(2)]),
            Ret::Int(-1)
        );
        assert_eq!(
            call(Intrinsic::StrSlice, &[text, Arg::Int(0), Arg::Int(3)]),
            Ret::Str("hé".to_owned())
        );
        assert_eq!(
            call(Intrinsic::StrSlice, &[text, Arg::Int(0), Arg::Int(2)]),
            Ret::Str(String::new())
        );
        assert_eq!(
            call(Intrinsic::StrIsBoundary, &[text, Arg::Int(2)]),
            Ret::Bool(false)
        );
        assert_eq!(
            call(Intrinsic::StrCharAt, &[text, Arg::Int(1)]),
            Ret::Char('é')
        );
        assert_eq!(call(Intrinsic::StrToInt, &[Arg::Str("-42")]), Ret::Int(-42));
        assert_eq!(
            call(Intrinsic::StrIsInt, &[Arg::Str("4x")]),
            Ret::Bool(false)
        );
        assert_eq!(
            call(Intrinsic::CharToUpper, &[Arg::Char('ß')]),
            Ret::Char('ß')
        );
        assert_eq!(
            call(Intrinsic::CharToUpper, &[Arg::Char('a')]),
            Ret::Char('A')
        );
    }

    #[test]
    fn numbers() {
        assert_eq!(
            call(
                Intrinsic::IntToStringBase,
                &[Arg::Int(i64::MIN), Arg::Int(16)]
            ),
            Ret::Str("-8000000000000000".to_owned())
        );
        assert_eq!(
            call(Intrinsic::IntToStringBase, &[Arg::Int(255), Arg::Int(2)]),
            Ret::Str("11111111".to_owned())
        );
        assert_eq!(
            call(Intrinsic::StrToIntBase, &[Arg::Str("-ff"), Arg::Int(16)]),
            Ret::Int(-255)
        );
        assert_eq!(
            call(Intrinsic::StrIsIntBase, &[Arg::Str("12"), Arg::Int(1)]),
            Ret::Bool(false)
        );
        assert_eq!(
            call(Intrinsic::RandomBelow, &[Arg::Int(-1), Arg::Int(10)]),
            Ret::Int(-1)
        );
        assert_eq!(
            call(Intrinsic::RandomBelow, &[Arg::Int(25), Arg::Int(10)]),
            Ret::Int(5)
        );
        assert_eq!(
            call(
                Intrinsic::FloatToStringFixed,
                &[Arg::Float(2.0 / 3.0), Arg::Int(3)]
            ),
            Ret::Str("0.667".to_owned())
        );
        assert_eq!(call(Intrinsic::Gamma, &[Arg::Float(5.0)]), Ret::Float(24.0));
    }
}
