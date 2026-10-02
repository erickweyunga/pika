//! Runtime support for compiled Pika programs.
//!
//! The [`format`](mod@crate::format) module defines how values are printed, and [`PanicKind`] what runtime errors
//! say. Both the compiler's interpreter and compiled code use them, so that a program prints
//! exactly the same text however it runs. The [`abi`] module holds the functions that compiled
//! code calls.

pub mod abi;
pub mod collections;
pub mod format;
pub mod heap;
pub mod intrinsics;
pub mod string;

use std::sync::Mutex;

/// Why a program panicked (spec section 9.3). The discriminants are part of the interface
/// between compiled code and the runtime.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u32)]
pub enum PanicKind {
    /// `:panic message`. The message is printed by the program.
    Explicit = 0,
    /// A failed `:assert`. The message is printed by the program.
    Assertion = 1,
    /// An arithmetic result that does not fit its type.
    Overflow = 2,
    /// Division or remainder by zero.
    DivisionByZero = 3,
    /// A shift by at least the bit width of the type.
    ShiftOverflow = 4,
    /// An integer conversion with `as` that would lose information.
    LossyCast = 5,
    /// A `:for` loop with a step of zero.
    ZeroStep = 6,
    /// Recursion deeper than the stack allows.
    StackOverflow = 7,
    /// `[$option->unwrap]` on `none`.
    UnwrapNone = 8,
    /// An index outside a list. The message, with the index and length, is printed by the
    /// program.
    IndexOutOfBounds = 9,
    /// A map read with a key it does not have.
    MissingKey = 10,
}

impl PanicKind {
    /// The kind with the given discriminant.
    pub fn from_code(code: u32) -> Option<Self> {
        Some(match code {
            0 => Self::Explicit,
            1 => Self::Assertion,
            2 => Self::Overflow,
            3 => Self::DivisionByZero,
            4 => Self::ShiftOverflow,
            5 => Self::LossyCast,
            6 => Self::ZeroStep,
            7 => Self::StackOverflow,
            8 => Self::UnwrapNone,
            9 => Self::IndexOutOfBounds,
            10 => Self::MissingKey,
            _ => return None,
        })
    }

    /// The text printed before a message provided by the program (`:panic`, `:assert`).
    pub fn message_prefix(self) -> &'static str {
        match self {
            Self::Assertion => "assertion failed: ",
            Self::IndexOutOfBounds => "index out of bounds: ",
            _ => "",
        }
    }

    /// The fixed message of a runtime error, or the prefix of a program-provided message.
    pub fn message(self) -> &'static str {
        match self {
            Self::Explicit => "",
            Self::Assertion => "assertion failed",
            Self::Overflow => "integer overflow",
            Self::DivisionByZero => "division by zero",
            Self::ShiftOverflow => "shift amount is not smaller than the bit width",
            Self::LossyCast => "conversion with `as` loses information",
            Self::ZeroStep => "the step of a `:for` loop is zero",
            Self::StackOverflow => "stack overflow",
            Self::UnwrapNone => "`unwrap` of `none`",
            Self::IndexOutOfBounds => "index out of bounds",
            Self::MissingKey => "key not found in the map",
        }
    }
}

/// The exit status of a program that panicked.
pub const PANIC_EXIT_CODE: i32 = 101;

/// The exit status of a program whose `main` raised an error.
pub const ERROR_EXIT_CODE: i32 = 1;

/// The names of the source files of the running program, which panic locations refer to by
/// index.
static PROGRAM_FILES: Mutex<Vec<String>> = Mutex::new(Vec::new());

/// Sets the names of the source files that panic locations refer to.
pub fn set_program_files(names: Vec<String>) {
    *PROGRAM_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = names;
}

/// The name of the source file at `index`, for a panic location.
pub fn program_file(index: u32) -> String {
    PROGRAM_FILES
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .get(index as usize)
        .cloned()
        .unwrap_or_else(|| "<unknown>".to_owned())
}
