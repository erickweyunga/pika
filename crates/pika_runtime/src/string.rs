//! Strings of compiled programs.
//!
//! A string is a [`PikaString`]: a pointer, a length and a capacity. Literals point at static
//! data with a capacity of 0, so they need no allocation; any change copies the bytes to the
//! heap first. A string owns its buffer exactly when its capacity is not 0.

#![allow(
    unsafe_code,
    reason = "strings are raw buffers shared with compiled code"
)]

use std::alloc::{Layout, alloc, dealloc, realloc};

use crate::{format, heap};

/// The memory layout of a string, shared with compiled code: three machine words.
#[repr(C)]
#[derive(Debug)]
pub struct PikaString {
    /// The first byte. Dangling (but non-null) for an empty string without a buffer.
    pub ptr: *mut u8,
    /// The number of bytes of UTF-8 text.
    pub len: usize,
    /// The size of the owned heap buffer, or 0 if the bytes are not owned (static data).
    pub cap: usize,
}

fn buffer_layout(cap: usize) -> Layout {
    Layout::array::<u8>(cap).expect("string capacity overflow")
}

impl PikaString {
    /// The text of the string.
    ///
    /// # Safety
    ///
    /// The string must be valid: `ptr` points to `len` bytes of UTF-8 that are alive.
    pub unsafe fn as_str<'a>(&self) -> &'a str {
        if self.len == 0 {
            return "";
        }
        // SAFETY: guaranteed by the caller; compiled code only creates valid strings.
        unsafe { std::str::from_utf8_unchecked(std::slice::from_raw_parts(self.ptr, self.len)) }
    }

    /// Makes room for `additional` more bytes in an owned buffer.
    ///
    /// # Safety
    ///
    /// The string must be valid.
    unsafe fn reserve(&mut self, additional: usize) {
        let needed = self
            .len
            .checked_add(additional)
            .expect("string length overflow");
        if self.cap >= needed {
            return;
        }
        let new_cap = needed.max(self.cap.saturating_mul(2)).max(16);
        let new_ptr = if self.cap == 0 {
            // Not owned (static or empty): copy the existing bytes into a new buffer.
            // SAFETY: the layout has a non-zero size.
            let new_ptr = unsafe { alloc(buffer_layout(new_cap)) };
            assert!(!new_ptr.is_null(), "out of memory");
            if self.len > 0 {
                // SAFETY: both regions are valid for `len` bytes and do not overlap.
                unsafe { std::ptr::copy_nonoverlapping(self.ptr, new_ptr, self.len) };
            }
            heap::allocated();
            new_ptr
        } else {
            // SAFETY: `ptr` was allocated with `buffer_layout(cap)`.
            let new_ptr = unsafe { realloc(self.ptr, buffer_layout(self.cap), new_cap) };
            assert!(!new_ptr.is_null(), "out of memory");
            new_ptr
        };
        self.ptr = new_ptr;
        self.cap = new_cap;
    }

    /// Appends text.
    ///
    /// # Safety
    ///
    /// The string must be valid.
    pub unsafe fn push_str(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        // SAFETY: the string is valid; `reserve` makes room for the bytes.
        unsafe {
            self.reserve(text.len());
            std::ptr::copy_nonoverlapping(text.as_ptr(), self.ptr.add(self.len), text.len());
        }
        self.len += text.len();
    }
}

/// A new string holding a copy of `text`, in a heap buffer unless it is empty.
pub fn owned(text: &str) -> PikaString {
    let mut string = empty();
    // SAFETY: an empty string is valid.
    unsafe { string.push_str(text) };
    string
}

fn empty() -> PikaString {
    PikaString {
        ptr: std::ptr::NonNull::<u8>::dangling().as_ptr(),
        len: 0,
        cap: 0,
    }
}

/// Initializes `dest` as an empty string.
///
/// # Safety
///
/// `dest` must point to writable memory for a `PikaString`.
pub unsafe extern "C" fn pika_string_new(dest: *mut PikaString) {
    // SAFETY: guaranteed by the caller.
    unsafe { dest.write(empty()) };
}

/// Frees the buffer of a string, if it owns one.
///
/// # Safety
///
/// `string` must point to a valid string that is not used afterwards.
pub unsafe extern "C" fn pika_string_drop(string: *mut PikaString) {
    // SAFETY: guaranteed by the caller.
    let string = unsafe { &mut *string };
    if string.cap > 0 {
        // SAFETY: an owned buffer was allocated with `buffer_layout(cap)`.
        unsafe { dealloc(string.ptr, buffer_layout(string.cap)) };
        heap::freed();
        string.cap = 0;
        string.len = 0;
    }
}

/// Initializes `dest` as an owned copy of `source`.
///
/// # Safety
///
/// `dest` must be writable memory for a string; `source` must be a valid string.
pub unsafe extern "C" fn pika_string_clone(dest: *mut PikaString, source: *const PikaString) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let text = (*source).as_str();
        let mut copy = empty();
        copy.push_str(text);
        dest.write(copy);
    }
}

/// Appends UTF-8 bytes.
///
/// # Safety
///
/// `dest` must be a valid string; `ptr` must point to `len` bytes of UTF-8.
pub unsafe extern "C" fn pika_string_push_bytes(dest: *mut PikaString, ptr: *const u8, len: usize) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let text = std::str::from_utf8_unchecked(std::slice::from_raw_parts(ptr, len));
        (*dest).push_str(text);
    }
}

/// Appends another string.
///
/// # Safety
///
/// Both must be valid strings; they may be the same string.
pub unsafe extern "C" fn pika_string_push_string(dest: *mut PikaString, source: *const PikaString) {
    // SAFETY: guaranteed by the caller. The text is copied before `dest` may reallocate.
    unsafe {
        let text = (*source).as_str().to_owned();
        (*dest).push_str(&text);
    }
}

/// Appends another string quoted, as in source (inside a displayed struct).
///
/// # Safety
///
/// Both must be valid strings; they may be the same string.
pub unsafe extern "C" fn pika_string_push_string_quoted(
    dest: *mut PikaString,
    source: *const PikaString,
) {
    // SAFETY: guaranteed by the caller. The text is copied before `dest` may reallocate.
    unsafe {
        let text = format::quote_string((*source).as_str());
        (*dest).push_str(&text);
    }
}

macro_rules! push_formatted {
    ($name:ident, $ty:ty, $format:expr) => {
        /// Appends a value formatted for display.
        ///
        /// # Safety
        ///
        /// `dest` must be a valid string.
        pub unsafe extern "C" fn $name(dest: *mut PikaString, value: $ty) {
            let text = $format(value);
            // SAFETY: guaranteed by the caller.
            unsafe { (*dest).push_str(&text) };
        }
    };
}

push_formatted!(pika_string_push_i64, i64, |v: i64| v.to_string());
push_formatted!(pika_string_push_u64, u64, |v: u64| v.to_string());
push_formatted!(pika_string_push_f64, f64, format::f64_to_string);
push_formatted!(pika_string_push_f32, f32, format::f32_to_string);
push_formatted!(pika_string_push_bool, u8, |v: u8| if v == 0 {
    "false"
} else {
    "true"
}
.to_owned());
push_formatted!(pika_string_push_char, u32, |v: u32| char::from_u32(v)
    .unwrap_or('\u{FFFD}')
    .to_string());
push_formatted!(pika_string_push_char_quoted, u32, |v: u32| {
    format::quote_char(char::from_u32(v).unwrap_or('\u{FFFD}'))
});
push_formatted!(pika_string_push_duration, i64, format::duration_to_string);

/// Compares two strings by their bytes: negative, zero or positive.
///
/// # Safety
///
/// Both must be valid strings.
pub unsafe extern "C" fn pika_string_compare(a: *const PikaString, b: *const PikaString) -> i32 {
    // SAFETY: guaranteed by the caller.
    let (a, b) = unsafe { ((*a).as_str(), (*b).as_str()) };
    match a.cmp(b) {
        std::cmp::Ordering::Less => -1,
        std::cmp::Ordering::Equal => 0,
        std::cmp::Ordering::Greater => 1,
    }
}

/// Returns 1 if `haystack` contains `needle`, else 0.
///
/// # Safety
///
/// Both must be valid strings.
pub unsafe extern "C" fn pika_string_contains(
    haystack: *const PikaString,
    needle: *const PikaString,
) -> u8 {
    // SAFETY: guaranteed by the caller.
    let (haystack, needle) = unsafe { ((*haystack).as_str(), (*needle).as_str()) };
    u8::from(haystack.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{COUNTER, live_allocations};

    fn text(string: &PikaString) -> String {
        // SAFETY: the tests only create valid strings.
        unsafe { string.as_str() }.to_owned()
    }

    #[test]
    fn building_and_freeing() {
        let _guard = COUNTER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = live_allocations();
        let mut string = empty();
        // SAFETY: `string` is valid throughout.
        unsafe {
            pika_string_push_bytes(&raw mut string, b"abc".as_ptr(), 3);
            pika_string_push_i64(&raw mut string, -5);
            pika_string_push_duration(&raw mut string, 90_000_000_000);
            let alias: *const PikaString = &raw const string;
            pika_string_push_string(&raw mut string, alias);
        }
        assert_eq!(text(&string), "abc-51m30sabc-51m30s");
        assert_eq!(live_allocations(), before + 1);
        let mut copy = empty();
        // SAFETY: both strings are valid.
        unsafe {
            pika_string_clone(&raw mut copy, &raw const string);
            assert_eq!(pika_string_compare(&raw const copy, &raw const string), 0);
            pika_string_drop(&raw mut string);
            pika_string_drop(&raw mut copy);
        }
        assert_eq!(live_allocations(), before);
    }

    #[test]
    fn static_strings_are_copied_before_changing() {
        static TEXT: &[u8] = b"static";
        let _guard = COUNTER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = live_allocations();
        let mut string = PikaString {
            ptr: TEXT.as_ptr().cast_mut(),
            len: TEXT.len(),
            cap: 0,
        };
        // SAFETY: the string is valid; dropping a static string does nothing.
        unsafe {
            pika_string_drop(&raw mut string);
            assert_eq!(live_allocations(), before);
            pika_string_push_bytes(&raw mut string, b"!".as_ptr(), 1);
        }
        assert_eq!(text(&string), "static!");
        assert_eq!(TEXT, b"static");
        // SAFETY: the string is valid and owned now.
        unsafe { pika_string_drop(&raw mut string) };
        assert_eq!(live_allocations(), before);
    }

    #[test]
    fn comparisons() {
        let make = |s: &'static str| PikaString {
            ptr: s.as_ptr().cast_mut(),
            len: s.len(),
            cap: 0,
        };
        let (a, b, hello, ell, empty, hi) = (
            make("a"),
            make("b"),
            make("hello"),
            make("ell"),
            make(""),
            make("hi"),
        );
        // SAFETY: static strings are valid.
        unsafe {
            assert_eq!(pika_string_compare(&raw const a, &raw const b), -1);
            assert_eq!(pika_string_compare(&raw const b, &raw const a), 1);
            assert_eq!(pika_string_contains(&raw const hello, &raw const ell), 1);
            assert_eq!(pika_string_contains(&raw const hello, &raw const empty), 1);
            assert_eq!(pika_string_contains(&raw const hi, &raw const hello), 0);
        }
    }
}
