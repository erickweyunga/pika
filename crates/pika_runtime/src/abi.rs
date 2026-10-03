//! Functions called by compiled Pika code.
//!
//! These use the C calling convention and only plain scalar arguments, so that code generated
//! by Cranelift can call them directly. Output to standard output is buffered and flushed when
//! the program ends ([`finish`]) or panics.

#![allow(unsafe_code, reason = "compiled code passes strings as raw pointers")]

use std::io::{BufWriter, Stdout, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{LazyLock, Mutex, PoisonError};

use crate::intrinsics::{self, Arg, Intrinsic, Io, Kind, Ret};
use crate::string::{self, PikaString};
use crate::{PANIC_EXIT_CODE, PanicKind, collections, format, heap, program_file, program_line};

/// Stream number of standard output.
pub const STDOUT: u32 = 1;
/// Stream number of standard error.
pub const STDERR: u32 = 2;

static OUT: LazyLock<Mutex<BufWriter<Stdout>>> =
    LazyLock::new(|| Mutex::new(BufWriter::new(std::io::stdout())));

fn write(stream: u32, text: &str) {
    // Output errors (such as a closed pipe) cannot be reported anywhere useful; a program
    // whose output is gone keeps running, like a shell command writing to a closed pipe
    // after ignoring SIGPIPE.
    if stream == STDERR {
        let _ = std::io::stderr().write_all(text.as_bytes());
    } else {
        let _ = OUT
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .write_all(text.as_bytes());
    }
}

/// Flushes buffered standard output. Called when the program's `main` returns.
pub fn finish() {
    let _ = OUT.lock().unwrap_or_else(PoisonError::into_inner).flush();
}

/// The exit status of a program that finished but leaked memory, under a leak check.
pub const LEAK_EXIT_CODE: i32 = 102;

/// When the environment variable `PIKA_LEAK_CHECK` is set, reports heap buffers that the
/// program did not free and returns the leak exit status. The compiler's own tests use this
/// to verify that compiled programs free everything they allocate.
pub fn leak_check() -> Option<i32> {
    std::env::var_os("PIKA_LEAK_CHECK")?;
    let live = heap::live_allocations();
    if live == 0 {
        return None;
    }
    write(
        STDERR,
        &format!("leak check: {live} heap allocation(s) were not freed\n"),
    );
    Some(LEAK_EXIT_CODE)
}

/// The lowest stack address compiled code may use; below it, calls report a stack overflow.
/// Compiled functions read it in their prologue.
#[unsafe(export_name = "pika_stack_limit")]
pub static STACK_LIMIT: AtomicUsize = AtomicUsize::new(0);

/// Sets the stack limit for a program running on the current thread, whose stack is
/// `stack_size` bytes. A margin is kept for the runtime's own calls.
pub fn set_stack_limit(stack_size: usize) {
    let marker = 0u8;
    let current = std::ptr::from_ref(&marker) as usize;
    let margin = 256 * 1024;
    STACK_LIMIT.store(
        current.saturating_sub(stack_size.saturating_sub(margin)),
        Ordering::Relaxed,
    );
}

/// Prints a string value.
///
/// # Safety
///
/// `string` must point to a valid string.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pika_print_string(string: *const PikaString, stream: u32) {
    // SAFETY: guaranteed by the caller.
    write(stream, unsafe { (*string).as_str() });
}

/// Prints a signed integer.
#[unsafe(no_mangle)]
pub extern "C" fn pika_print_i64(value: i64, stream: u32) {
    write(stream, &value.to_string());
}

/// Prints an unsigned integer.
#[unsafe(no_mangle)]
pub extern "C" fn pika_print_u64(value: u64, stream: u32) {
    write(stream, &value.to_string());
}

/// Prints a 64-bit float.
#[unsafe(no_mangle)]
pub extern "C" fn pika_print_f64(value: f64, stream: u32) {
    write(stream, &format::f64_to_string(value));
}

/// Prints a 32-bit float.
#[unsafe(no_mangle)]
pub extern "C" fn pika_print_f32(value: f32, stream: u32) {
    write(stream, &format::f32_to_string(value));
}

/// Prints a boolean (0 or 1).
#[unsafe(no_mangle)]
pub extern "C" fn pika_print_bool(value: u8, stream: u32) {
    write(stream, if value == 0 { "false" } else { "true" });
}

/// Prints a character given as a Unicode scalar value.
#[unsafe(no_mangle)]
pub extern "C" fn pika_print_char(value: u32, stream: u32) {
    write(
        stream,
        &char::from_u32(value).unwrap_or('\u{FFFD}').to_string(),
    );
}

/// Prints a duration given in nanoseconds.
#[unsafe(no_mangle)]
pub extern "C" fn pika_print_duration(nanos: i64, stream: u32) {
    write(stream, &format::duration_to_string(nanos));
}

/// Prints UTF-8 text.
///
/// # Safety
///
/// `ptr` must point to `len` bytes of valid UTF-8 that stay alive for the duration of the
/// call. Compiled code only passes string constants from its data section.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pika_print_str(ptr: *const u8, len: usize, stream: u32) {
    // SAFETY: guaranteed by the caller, see above.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    write(stream, &String::from_utf8_lossy(bytes));
}

/// Prints a line break.
#[unsafe(no_mangle)]
pub extern "C" fn pika_print_newline(stream: u32) {
    write(stream, "\n");
}

/// Starts reporting a panic whose message the program prints next, to standard error.
#[unsafe(no_mangle)]
pub extern "C" fn pika_panic_begin(kind: u32) {
    finish();
    let prefix = PanicKind::from_code(kind).map_or("", PanicKind::message_prefix);
    write(STDERR, "panic: ");
    write(STDERR, prefix);
}

/// Finishes reporting a panic started by [`pika_panic_begin`], at line `line` and column
/// `column` of the source file at index `file`, and exits.
#[unsafe(no_mangle)]
pub extern "C" fn pika_panic_end(file: u32, line: u32, column: u32) -> ! {
    let name = program_file(file);
    let source = program_line(&name, line);
    write(STDERR, "\n");
    write(
        STDERR,
        &format::location(&name, line, column, source.as_deref()),
    );
    std::process::exit(PANIC_EXIT_CODE);
}

/// Reports a runtime error with a fixed message, at line `line` and column `column` of the
/// source file at index `file`, and exits.
#[unsafe(no_mangle)]
pub extern "C" fn pika_panic(kind: u32, file: u32, line: u32, column: u32) -> ! {
    finish();
    let message = PanicKind::from_code(kind).map_or("unknown error", PanicKind::message);
    let name = program_file(file);
    let source = program_line(&name, line);
    write(
        STDERR,
        &format::panic_report(message, &name, line, column, source.as_deref()),
    );
    std::process::exit(PANIC_EXIT_CODE);
}

/// Where the parts of an `Error` value are, in bytes from its start; compiled code passes them
/// to [`pika_uncaught`].
#[repr(C)]
pub struct ErrorLayout {
    /// The message, a string.
    pub message: u32,
    /// The source, a `Box<Error>?`.
    pub source: u32,
    /// The box in the source, from the start of the source.
    pub source_box: u32,
    /// The file, a string.
    pub file: u32,
    /// The line, a `u32`.
    pub line: u32,
    /// The column, a `u32`.
    pub column: u32,
    /// The trace, a `List<String>`.
    pub trace: u32,
}

/// Reports an error raised by `main` and exits with status 1.
///
/// # Safety
///
/// `error` must point to a valid `Error` value laid out as `layout` describes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pika_uncaught(error: *const u8, layout: *const ErrorLayout) -> ! {
    // SAFETY: guaranteed by the caller.
    let layout = unsafe { &*layout };
    let mut errors = Vec::new();
    let mut current = error;
    loop {
        // SAFETY: `current` points to a valid `Error`, whose parts are at the given offsets;
        // its source is either none (tag 0) or a box holding another valid `Error`.
        unsafe {
            let field = |offset: u32| current.add(offset as usize);
            let message = field(layout.message)
                .cast::<PikaString>()
                .read_unaligned()
                .as_str();
            let file = field(layout.file)
                .cast::<PikaString>()
                .read_unaligned()
                .as_str();
            let line = field(layout.line).cast::<u32>().read_unaligned();
            let column = field(layout.column).cast::<u32>().read_unaligned();
            let trace_list = field(layout.trace)
                .cast::<collections::RawList>()
                .read_unaligned();
            let trace = (0..trace_list.len)
                .map(|i| {
                    trace_list
                        .ptr
                        .add(i * std::mem::size_of::<PikaString>())
                        .cast::<PikaString>()
                        .read_unaligned()
                        .as_str()
                        .to_owned()
                })
                .collect();
            errors.push(format::RaisedError {
                message: message.to_owned(),
                file: file.to_owned(),
                line,
                column,
                trace,
            });
            let tag = field(layout.source).cast::<u32>().read_unaligned();
            if tag == 0 {
                break;
            }
            current = field(layout.source + layout.source_box)
                .cast::<*const u8>()
                .read_unaligned();
        }
    }
    finish();
    write(
        STDERR,
        &format::error_report(&errors, &|name, line| program_line(name, line)),
    );
    std::process::exit(crate::ERROR_EXIT_CODE);
}

/// Floating-point remainder, which Cranelift has no instruction for: the result has the sign
/// of the dividend.
#[unsafe(no_mangle)]
pub extern "C" fn pika_rem_f64(a: f64, b: f64) -> f64 {
    a % b
}

/// Floating-point remainder for `f32`.
#[unsafe(no_mangle)]
pub extern "C" fn pika_rem_f32(a: f32, b: f32) -> f32 {
    a % b
}

/// The process's streams, through the buffer that `:put` writes to.
struct ProcessIo;

impl Io for ProcessIo {
    fn write_out(&mut self, text: &str) {
        write(STDOUT, text);
    }

    fn write_err(&mut self, text: &str) {
        write(STDERR, text);
    }

    fn read_line(&mut self) -> String {
        finish();
        intrinsics::read_stdin_line()
    }
}

/// Runs the intrinsic with discriminant `code`. `args` points to one pointer per parameter,
/// to the argument's value: an `i64` (also for `Duration`), an `f64`, a `bool` as a byte, a
/// `char` as a `u32`, or a string. The result is written to `out` in the same way; a string
/// result is a new string that the caller owns.
///
/// # Panics
///
/// Panics if `code` is not the discriminant of an intrinsic, which compiled code ensures.
///
/// # Safety
///
/// `code` must be the discriminant of an intrinsic, `args` must point to one valid pointer
/// per parameter, each to a valid value of the parameter's kind, and `out` must be writable
/// for a value of the result's kind.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn pika_intrinsic(code: u32, args: *const *const u8, out: *mut u8) {
    let intrinsic = Intrinsic::from_code(code).expect("compiled code passes valid intrinsics");
    let values: Vec<Arg<'_>> = intrinsic
        .params()
        .iter()
        .enumerate()
        // SAFETY: guaranteed by the caller.
        .map(|(index, kind)| unsafe {
            let value = *args.add(index);
            match kind {
                Kind::Int => Arg::Int(value.cast::<i64>().read_unaligned()),
                Kind::Duration => Arg::Duration(value.cast::<i64>().read_unaligned()),
                Kind::Float => Arg::Float(value.cast::<f64>().read_unaligned()),
                Kind::Bool => Arg::Bool(value.read() != 0),
                Kind::Char => Arg::Char(
                    char::from_u32(value.cast::<u32>().read_unaligned()).unwrap_or('\u{FFFD}'),
                ),
                Kind::Str => Arg::Str(value.cast::<PikaString>().read_unaligned().as_str()),
                Kind::Nothing => unreachable!("no parameter is `nothing`"),
            }
        })
        .collect();
    let result = intrinsic.call(&values, &mut ProcessIo);
    // SAFETY: guaranteed by the caller.
    unsafe {
        match result {
            Ret::Int(value) | Ret::Duration(value) => out.cast::<i64>().write_unaligned(value),
            Ret::Float(value) => out.cast::<f64>().write_unaligned(value),
            Ret::Bool(value) => out.write(u8::from(value)),
            Ret::Char(value) => out.cast::<u32>().write_unaligned(u32::from(value)),
            Ret::Str(text) => out
                .cast::<PikaString>()
                .write_unaligned(string::owned(&text)),
            Ret::Nothing => {}
        }
    }
}

/// The address of every runtime function, by symbol name, for registering with a JIT.
pub fn symbols() -> Vec<(&'static str, *const u8)> {
    vec![
        ("pika_print_i64", pika_print_i64 as *const u8),
        ("pika_print_u64", pika_print_u64 as *const u8),
        ("pika_print_f64", pika_print_f64 as *const u8),
        ("pika_print_f32", pika_print_f32 as *const u8),
        ("pika_print_bool", pika_print_bool as *const u8),
        ("pika_print_char", pika_print_char as *const u8),
        ("pika_print_duration", pika_print_duration as *const u8),
        ("pika_print_str", pika_print_str as *const u8),
        ("pika_print_newline", pika_print_newline as *const u8),
        ("pika_panic_begin", pika_panic_begin as *const u8),
        ("pika_panic_end", pika_panic_end as *const u8),
        ("pika_panic", pika_panic as *const u8),
        ("pika_uncaught", pika_uncaught as *const u8),
        ("pika_rem_f64", pika_rem_f64 as *const u8),
        ("pika_rem_f32", pika_rem_f32 as *const u8),
        ("pika_print_string", pika_print_string as *const u8),
        ("pika_string_new", string::pika_string_new as *const u8),
        ("pika_intrinsic", pika_intrinsic as *const u8),
        ("pika_alloc", heap::pika_alloc as *const u8),
        (
            "pika_list_reserve",
            collections::pika_list_reserve as *const u8,
        ),
        ("pika_list_free", collections::pika_list_free as *const u8),
        ("pika_list_open", collections::pika_list_open as *const u8),
        ("pika_list_close", collections::pika_list_close as *const u8),
        ("pika_map_find", collections::pika_map_find as *const u8),
        ("pika_map_push", collections::pika_map_push as *const u8),
        ("pika_map_remove", collections::pika_map_remove as *const u8),
        ("pika_map_clear", collections::pika_map_clear as *const u8),
        ("pika_map_free", collections::pika_map_free as *const u8),
        ("pika_hash_bytes", collections::pika_hash_bytes as *const u8),
        ("pika_free", heap::pika_free as *const u8),
        ("pika_string_drop", string::pika_string_drop as *const u8),
        ("pika_string_clone", string::pika_string_clone as *const u8),
        (
            "pika_string_push_bytes",
            string::pika_string_push_bytes as *const u8,
        ),
        (
            "pika_string_push_string",
            string::pika_string_push_string as *const u8,
        ),
        (
            "pika_string_push_i64",
            string::pika_string_push_i64 as *const u8,
        ),
        (
            "pika_string_push_u64",
            string::pika_string_push_u64 as *const u8,
        ),
        (
            "pika_string_push_f64",
            string::pika_string_push_f64 as *const u8,
        ),
        (
            "pika_string_push_f32",
            string::pika_string_push_f32 as *const u8,
        ),
        (
            "pika_string_push_bool",
            string::pika_string_push_bool as *const u8,
        ),
        (
            "pika_string_push_char",
            string::pika_string_push_char as *const u8,
        ),
        (
            "pika_string_push_duration",
            string::pika_string_push_duration as *const u8,
        ),
        (
            "pika_string_push_string_quoted",
            string::pika_string_push_string_quoted as *const u8,
        ),
        (
            "pika_string_push_char_quoted",
            string::pika_string_push_char_quoted as *const u8,
        ),
        (
            "pika_string_compare",
            string::pika_string_compare as *const u8,
        ),
        (
            "pika_string_contains",
            string::pika_string_contains as *const u8,
        ),
        (
            "pika_stack_limit",
            STACK_LIMIT.as_ptr().cast::<u8>().cast_const(),
        ),
    ]
}
