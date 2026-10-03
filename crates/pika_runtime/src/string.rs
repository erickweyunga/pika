//! Strings of compiled programs.
//!
//! A string is a [`PikaString`]: a pointer, a length and a capacity. Literals point at static
//! data with a capacity of 0, so they need no allocation; any change copies the bytes to the
//! heap first. A string with a capacity that is not 0 has a heap buffer, which copies of the
//! string share: a count of the strings sharing it is kept just before its bytes. Copying a
//! string adds to the count; changing a string whose buffer is shared first gives it a buffer
//! of its own (copy on write). Programs are single-threaded in v0, so the count is a plain
//! integer.

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

/// The size of the count of strings sharing a buffer, before its bytes.
const HEADER: usize = std::mem::size_of::<usize>();

/// The layout of a heap buffer for `cap` bytes, with its count.
fn buffer_layout(cap: usize) -> Layout {
    Layout::from_size_align(
        HEADER.checked_add(cap).expect("string capacity overflow"),
        std::mem::align_of::<usize>(),
    )
    .expect("string capacity overflow")
}

/// A new heap buffer for `cap` bytes, shared by one string; returns the address of its first
/// byte.
#[allow(
    clippy::cast_ptr_alignment,
    reason = "the buffer is aligned for `usize` by `buffer_layout`"
)]
fn new_buffer(cap: usize) -> *mut u8 {
    // SAFETY: the layout has a non-zero size.
    let base = unsafe { alloc(buffer_layout(cap)) };
    assert!(!base.is_null(), "out of memory");
    heap::allocated();
    // SAFETY: the buffer starts with room for the count, aligned for it.
    unsafe {
        base.cast::<usize>().write(1);
        base.add(HEADER)
    }
}

/// The count of strings sharing the heap buffer whose first byte is at `ptr`.
///
/// # Safety
///
/// `ptr` must be the first byte of a heap buffer made by [`new_buffer`].
#[allow(
    clippy::cast_ptr_alignment,
    reason = "the count starts a buffer aligned for `usize` by `buffer_layout`"
)]
unsafe fn count(ptr: *mut u8) -> *mut usize {
    // SAFETY: guaranteed by the caller.
    unsafe { ptr.sub(HEADER).cast::<usize>() }
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

    /// Makes room for `additional` more bytes in a heap buffer that only this string uses.
    ///
    /// # Safety
    ///
    /// The string must be valid.
    unsafe fn reserve(&mut self, additional: usize) {
        let needed = self
            .len
            .checked_add(additional)
            .expect("string length overflow");
        // SAFETY: a string with a capacity has a heap buffer with a count.
        let shared = self.cap > 0 && unsafe { *count(self.ptr) } > 1;
        if self.cap >= needed && !shared {
            return;
        }
        let new_cap = needed.max(self.cap.saturating_mul(2)).max(16);
        let new_ptr = if self.cap == 0 || shared {
            // Static, empty or shared: copy the bytes into a buffer of its own.
            let new_ptr = new_buffer(new_cap);
            if self.len > 0 {
                // SAFETY: both regions are valid for `len` bytes and do not overlap.
                unsafe { std::ptr::copy_nonoverlapping(self.ptr, new_ptr, self.len) };
            }
            if shared {
                // SAFETY: the buffer is shared, so the count stays above 0.
                unsafe { *count(self.ptr) -= 1 };
            }
            new_ptr
        } else {
            // SAFETY: the buffer was allocated with `buffer_layout(cap)`, and only this string
            // uses it; the count moves with it.
            unsafe {
                let base = realloc(
                    self.ptr.sub(HEADER),
                    buffer_layout(self.cap),
                    buffer_layout(new_cap).size(),
                );
                assert!(!base.is_null(), "out of memory");
                base.add(HEADER)
            }
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

/// Destroys a string: frees its heap buffer if no other string shares it.
///
/// # Safety
///
/// `string` must point to a valid string that is not used afterwards.
pub unsafe extern "C" fn pika_string_drop(string: *mut PikaString) {
    // SAFETY: guaranteed by the caller.
    let string = unsafe { &mut *string };
    if string.cap > 0 {
        // SAFETY: a heap buffer has a count, and was allocated with `buffer_layout(cap)`.
        unsafe {
            let shared = count(string.ptr);
            *shared -= 1;
            if *shared == 0 {
                dealloc(string.ptr.sub(HEADER), buffer_layout(string.cap));
                heap::freed();
            }
        }
        string.cap = 0;
        string.len = 0;
    }
}

/// Initializes `dest` as a copy of `source`, which shares its heap buffer.
///
/// # Safety
///
/// `dest` must be writable memory for a string; `source` must be a valid string.
pub unsafe extern "C" fn pika_string_clone(dest: *mut PikaString, source: *const PikaString) {
    // SAFETY: guaranteed by the caller; a heap buffer has a count.
    unsafe {
        let source = &*source;
        if source.cap > 0 {
            *count(source.ptr) += 1;
        }
        dest.write(PikaString {
            ptr: source.ptr,
            len: source.len,
            cap: source.cap,
        });
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

#[cfg(test)]
mod sharing_tests {
    use super::*;

    #[test]
    fn copies_share_a_buffer_until_one_changes() {
        let mut original = owned("shared text");
        let mut copy = empty();
        // SAFETY: both strings are valid, and each is dropped once.
        unsafe {
            pika_string_clone(&raw mut copy, &raw const original);
            assert_eq!(copy.ptr, original.ptr, "a copy shares the buffer");
            assert_eq!(*count(original.ptr), 2);

            copy.push_str("!");
            assert_ne!(
                copy.ptr, original.ptr,
                "a change gives the copy its own buffer"
            );
            assert_eq!(*count(original.ptr), 1);
            assert_eq!(copy.as_str(), "shared text!");
            assert_eq!(original.as_str(), "shared text");

            pika_string_drop(&raw mut copy);
            pika_string_drop(&raw mut original);
        }
    }

    #[test]
    fn the_last_copy_frees_the_buffer() {
        let mut first = owned("text");
        let mut second = empty();
        // SAFETY: both strings are valid, and each is dropped once.
        unsafe {
            pika_string_clone(&raw mut second, &raw const first);
            pika_string_drop(&raw mut first);
            assert_eq!(*count(second.ptr), 1, "the other copy keeps the buffer");
            assert_eq!(second.as_str(), "text");
            pika_string_drop(&raw mut second);
        }
    }

    #[test]
    fn static_strings_are_not_counted() {
        let text = "static";
        let literal = PikaString {
            ptr: text.as_ptr().cast_mut(),
            len: text.len(),
            cap: 0,
        };
        let mut copy = empty();
        // SAFETY: the literal points at static data; the copy shares it and frees nothing.
        unsafe {
            pika_string_clone(&raw mut copy, &raw const literal);
            assert_eq!(copy.cap, 0);
            assert_eq!(copy.as_str(), "static");
            pika_string_drop(&raw mut copy);
        }
    }
}
