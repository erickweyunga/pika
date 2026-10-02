//! Heap allocation for compiled programs, and accounting of live allocations.
//!
//! Every allocation made for a program, by strings and by boxes, is counted, so that a leak
//! check can verify that a program freed everything it allocated.

#![allow(unsafe_code, reason = "raw allocations shared with compiled code")]

use std::alloc::{Layout, alloc, dealloc};
use std::sync::atomic::{AtomicI64, Ordering};

/// Number of heap allocations currently live.
static LIVE_ALLOCATIONS: AtomicI64 = AtomicI64::new(0);

/// The number of heap allocations made and not yet freed, for leak checking.
pub fn live_allocations() -> i64 {
    LIVE_ALLOCATIONS.load(Ordering::Relaxed)
}

/// Records a new allocation.
pub(crate) fn allocated() {
    LIVE_ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
}

/// Records that an allocation was freed.
pub(crate) fn freed() {
    LIVE_ALLOCATIONS.fetch_sub(1, Ordering::Relaxed);
}

/// The layout of an allocation of `size` bytes aligned to `align`. Zero-sized values still
/// get a distinct allocation of one byte.
fn layout(size: usize, align: usize) -> Layout {
    Layout::from_size_align(size.max(1), align).expect("valid size and alignment")
}

/// Allocates memory for a boxed value.
///
/// # Panics
///
/// Panics (aborting the program) when out of memory or when `align` is not a power of two.
pub extern "C" fn pika_alloc(size: usize, align: usize) -> *mut u8 {
    // SAFETY: the layout has a non-zero size.
    let ptr = unsafe { alloc(layout(size, align)) };
    assert!(!ptr.is_null(), "out of memory");
    allocated();
    ptr
}

/// Frees memory allocated by [`pika_alloc`].
///
/// # Safety
///
/// `ptr` must come from `pika_alloc(size, align)` with the same size and alignment, and not
/// be used afterwards.
pub unsafe extern "C" fn pika_free(ptr: *mut u8, size: usize, align: usize) {
    // SAFETY: guaranteed by the caller.
    unsafe { dealloc(ptr, layout(size, align)) };
    freed();
}

/// Serializes tests that check the global allocation counter.
#[cfg(test)]
pub(crate) static COUNTER: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn allocations_are_counted() {
        let _guard = COUNTER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = live_allocations();
        let ptr = pika_alloc(24, 8);
        assert_eq!(live_allocations(), before + 1);
        assert_eq!(ptr.align_offset(8), 0);
        // SAFETY: allocated just above with the same layout.
        unsafe { pika_free(ptr, 24, 8) };
        let empty = pika_alloc(0, 1);
        // SAFETY: allocated just above with the same layout.
        unsafe { pika_free(empty, 0, 1) };
        assert_eq!(live_allocations(), before);
    }
}
