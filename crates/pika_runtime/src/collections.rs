//! Lists and maps of compiled programs, as raw memory.
//!
//! The runtime only manages buffers: it knows the size and alignment of elements, which the
//! compiled code passes in, but not their types. Compiled code reads, writes, moves, clones
//! and destroys the elements itself.
//!
//! A [`RawList`] is a buffer of elements and a length. A [`RawMap`] keeps its entries, in
//! insertion order, in a `RawList`; each entry starts with the hash of its key (a `u64`),
//! followed by the key and the value at offsets chosen by the compiled code. An index table
//! of entry positions, with open addressing, finds keys: compiled code computes hashes and
//! passes a function that compares two keys. Sets are maps whose values have no data.

#![allow(unsafe_code, reason = "raw buffers shared with compiled code")]

use std::alloc::{Layout, alloc, dealloc, realloc};

use crate::heap;

/// A list: a buffer of `cap` elements, of which the first `len` are initialized. The
/// buffer is null when `cap` is 0; elements without data need no buffer.
#[repr(C)]
#[derive(Debug)]
pub struct RawList {
    /// The buffer.
    pub ptr: *mut u8,
    /// The number of elements.
    pub len: usize,
    /// The number of elements the buffer has room for.
    pub cap: usize,
}

/// A map or set: its entries in insertion order, and an index table over them.
#[repr(C)]
#[derive(Debug)]
pub struct RawMap {
    /// The entries: each a `u64` hash, then the key and the value.
    pub entries: RawList,
    /// Slots holding an entry's position plus one, or 0 for an empty slot. Null when
    /// `table_cap` is 0.
    pub table: *mut u32,
    /// The number of slots, a power of two (or 0).
    pub table_cap: usize,
}

/// Compares two keys: nonzero if they are equal.
pub type KeyEq = extern "C" fn(*const u8, *const u8) -> u8;

fn buffer_layout(cap: usize, size: usize, align: usize) -> Layout {
    let bytes = cap.checked_mul(size).expect("collection too large");
    Layout::from_size_align(bytes, align).expect("valid collection layout")
}

impl RawList {
    /// Makes room for `additional` more elements.
    ///
    /// # Safety
    ///
    /// The list must be valid for elements of this size and alignment.
    unsafe fn reserve(&mut self, additional: usize, size: usize, align: usize) {
        let needed = self
            .len
            .checked_add(additional)
            .expect("collection too large");
        if size == 0 || needed <= self.cap {
            return;
        }
        let new_cap = needed.max(self.cap * 2).max(4);
        let new_layout = buffer_layout(new_cap, size, align);
        let ptr = if self.cap == 0 {
            // SAFETY: the layout has a non-zero size.
            let ptr = unsafe { alloc(new_layout) };
            heap::allocated();
            ptr
        } else {
            // SAFETY: the buffer was allocated with this layout for `cap` elements.
            unsafe {
                realloc(
                    self.ptr,
                    buffer_layout(self.cap, size, align),
                    new_layout.size(),
                )
            }
        };
        assert!(!ptr.is_null(), "out of memory");
        self.ptr = ptr;
        self.cap = new_cap;
    }

    /// Frees the buffer; the elements must have been destroyed.
    ///
    /// # Safety
    ///
    /// The list must be valid for elements of this size and alignment.
    unsafe fn free(&mut self, size: usize, align: usize) {
        if size != 0 && self.cap != 0 {
            // SAFETY: the buffer was allocated with this layout.
            unsafe { dealloc(self.ptr, buffer_layout(self.cap, size, align)) };
            heap::freed();
        }
        self.ptr = std::ptr::null_mut();
        self.len = 0;
        self.cap = 0;
    }

    /// The address of element `index`.
    fn element(&self, index: usize, size: usize) -> *mut u8 {
        self.ptr.wrapping_add(index * size)
    }
}

/// Makes room in a list for `additional` more elements of `size` bytes aligned to `align`.
///
/// # Safety
///
/// `list` must point to a valid list of such elements.
pub unsafe extern "C" fn pika_list_reserve(
    list: *mut RawList,
    additional: usize,
    size: usize,
    align: usize,
) {
    // SAFETY: guaranteed by the caller.
    unsafe { (*list).reserve(additional, size, align) };
}

/// Frees the buffer of a list whose elements have been destroyed.
///
/// # Safety
///
/// `list` must point to a valid list of such elements, not used afterwards.
pub unsafe extern "C" fn pika_list_free(list: *mut RawList, size: usize, align: usize) {
    // SAFETY: guaranteed by the caller.
    unsafe { (*list).free(size, align) };
}

/// Opens an uninitialized slot at `index` (at most the length), shifting later elements
/// up, and returns its address.
///
/// # Safety
///
/// `list` must point to a valid list of such elements, and `index <= len`.
pub unsafe extern "C" fn pika_list_open(
    list: *mut RawList,
    index: usize,
    size: usize,
    align: usize,
) -> *mut u8 {
    // SAFETY: guaranteed by the caller; the buffer has room for one more element.
    unsafe {
        let list = &mut *list;
        list.reserve(1, size, align);
        let slot = list.element(index, size);
        if size != 0 {
            std::ptr::copy(slot, slot.add(size), (list.len - index) * size);
        }
        list.len += 1;
        slot
    }
}

/// Closes the slot at `index` (whose element was moved out), shifting later elements down.
///
/// # Safety
///
/// `list` must point to a valid list of such elements, and `index < len`.
pub unsafe extern "C" fn pika_list_close(list: *mut RawList, index: usize, size: usize) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let list = &mut *list;
        let slot = list.element(index, size);
        if size != 0 {
            std::ptr::copy(slot.add(size), slot, (list.len - index - 1) * size);
        }
        list.len -= 1;
    }
}

/// Spreads the bits of a hash, so that the low bits of the result depend on all of them.
fn mix(hash: u64) -> u64 {
    let mut x = hash;
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    x ^ (x >> 31)
}

impl RawMap {
    /// The hash stored at the start of entry `index`.
    ///
    /// # Safety
    ///
    /// `index < len`, for entries of `entry_size` bytes.
    unsafe fn hash_at(&self, index: usize, entry_size: usize) -> u64 {
        // SAFETY: guaranteed by the caller; entries are aligned for their hash.
        unsafe {
            self.entries
                .element(index, entry_size)
                .cast::<u64>()
                .read_unaligned()
        }
    }

    /// The slots probed for `hash`, in order.
    fn probe(&self, hash: u64) -> impl Iterator<Item = usize> {
        let mask = self.table_cap - 1;
        let start = usize::try_from(mix(hash) & (mask as u64)).expect("masked");
        (0..self.table_cap).map(move |step| (start + step) & mask)
    }

    /// Records entry `index` in the table.
    ///
    /// # Safety
    ///
    /// The table has a free slot.
    unsafe fn index(&mut self, hash: u64, index: usize) {
        let position = u32::try_from(index + 1).expect("fewer than 2^32 entries");
        for slot in self.probe(hash) {
            // SAFETY: slots are within the table.
            unsafe {
                if self.table.add(slot).read() == 0 {
                    self.table.add(slot).write(position);
                    return;
                }
            }
        }
        unreachable!("the table always has a free slot");
    }

    /// Rebuilds the table with room for `len` entries.
    ///
    /// # Safety
    ///
    /// The map must be valid for entries of `entry_size` bytes.
    unsafe fn rebuild(&mut self, len: usize, entry_size: usize) {
        // At most half full.
        let cap = (len * 2).max(8).next_power_of_two();
        if cap != self.table_cap {
            // SAFETY: the old table, if any, was allocated with its layout.
            unsafe { self.free_table() };
            let layout = Layout::array::<u32>(cap).expect("table too large");
            // SAFETY: the layout has a non-zero size.
            #[allow(
                clippy::cast_ptr_alignment,
                reason = "allocated with the alignment of `u32`"
            )]
            let table = unsafe { alloc(layout) }.cast::<u32>();
            assert!(!table.is_null(), "out of memory");
            heap::allocated();
            self.table = table;
            self.table_cap = cap;
        }
        // SAFETY: the table has `table_cap` slots.
        unsafe { std::ptr::write_bytes(self.table, 0, self.table_cap) };
        for index in 0..self.entries.len {
            // SAFETY: `index < len`.
            let hash = unsafe { self.hash_at(index, entry_size) };
            // SAFETY: the table is at most half full.
            unsafe { self.index(hash, index) };
        }
    }

    /// Frees the table.
    ///
    /// # Safety
    ///
    /// The table, if any, was allocated by `rebuild`.
    unsafe fn free_table(&mut self) {
        if self.table_cap != 0 {
            let layout = Layout::array::<u32>(self.table_cap).expect("table too large");
            // SAFETY: allocated with this layout.
            unsafe { dealloc(self.table.cast::<u8>(), layout) };
            heap::freed();
            self.table = std::ptr::null_mut();
            self.table_cap = 0;
        }
    }
}

/// The position of the entry whose key equals the key at `key`, or -1. `key_offset` is
/// where keys are in an entry.
///
/// # Safety
///
/// `map` must point to a valid map of entries of `entry_size` bytes, `key` to a valid key,
/// and `eq` must compare two keys of its type.
///
/// # Panics
///
/// Never in practice: a map cannot have 2^63 entries.
pub unsafe extern "C" fn pika_map_find(
    map: *const RawMap,
    hash: u64,
    key: *const u8,
    eq: KeyEq,
    key_offset: usize,
    entry_size: usize,
) -> i64 {
    // SAFETY: guaranteed by the caller.
    let map = unsafe { &*map };
    if map.table_cap == 0 {
        return -1;
    }
    for slot in map.probe(hash) {
        // SAFETY: slots are within the table.
        let position = unsafe { map.table.add(slot).read() };
        if position == 0 {
            return -1;
        }
        let index = (position - 1) as usize;
        // SAFETY: indexed entries exist; the key is at `key_offset`.
        let found = unsafe {
            map.hash_at(index, entry_size) == hash
                && eq(map.entries.element(index, entry_size).add(key_offset), key) != 0
        };
        if found {
            return i64::try_from(index).expect("fewer than 2^63 entries");
        }
    }
    -1
}

/// Appends an entry with `hash` (whose key the caller checked is not in the map) and
/// returns its address; the caller writes the key and the value.
///
/// # Safety
///
/// `map` must point to a valid map of entries of `entry_size` bytes aligned to `align`.
pub unsafe extern "C" fn pika_map_push(
    map: *mut RawMap,
    hash: u64,
    entry_size: usize,
    align: usize,
) -> *mut u8 {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let map = &mut *map;
        map.entries.reserve(1, entry_size, align);
        let index = map.entries.len;
        let entry = map.entries.element(index, entry_size);
        entry.cast::<u64>().write_unaligned(hash);
        map.entries.len += 1;
        if map.entries.len * 2 > map.table_cap {
            map.rebuild(map.entries.len, entry_size);
        } else {
            map.index(hash, index);
        }
        entry
    }
}

/// Removes entry `index` (whose key and value were moved out or destroyed); later entries
/// keep their order.
///
/// # Safety
///
/// `map` must point to a valid map of entries of `entry_size` bytes, and `index < len`.
pub unsafe extern "C" fn pika_map_remove(map: *mut RawMap, index: usize, entry_size: usize) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let map = &mut *map;
        pika_list_close(&raw mut map.entries, index, entry_size);
        map.rebuild(map.entries.len, entry_size);
    }
}

/// Empties a map whose entries have been destroyed, keeping its buffers.
///
/// # Safety
///
/// `map` must point to a valid map.
pub unsafe extern "C" fn pika_map_clear(map: *mut RawMap) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let map = &mut *map;
        map.entries.len = 0;
        if map.table_cap != 0 {
            std::ptr::write_bytes(map.table, 0, map.table_cap);
        }
    }
}

/// Frees the buffers of a map whose entries have been destroyed.
///
/// # Safety
///
/// `map` must point to a valid map of entries of `entry_size` bytes aligned to `align`, not
/// used afterwards.
pub unsafe extern "C" fn pika_map_free(map: *mut RawMap, entry_size: usize, align: usize) {
    // SAFETY: guaranteed by the caller.
    unsafe {
        let map = &mut *map;
        map.entries.free(entry_size, align);
        map.free_table();
    }
}

/// The hash of `len` bytes, for strings.
///
/// # Safety
///
/// `ptr` must point to `len` readable bytes.
pub unsafe extern "C" fn pika_hash_bytes(ptr: *const u8, len: usize) -> u64 {
    if len == 0 {
        return 0x9e37_79b9_7f4a_7c15;
    }
    // SAFETY: guaranteed by the caller.
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len) };
    // FNV-1a.
    bytes.iter().fold(0xcbf2_9ce4_8422_2325, |hash, &byte| {
        (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::heap::{COUNTER, live_allocations};

    fn empty_map() -> RawMap {
        RawMap {
            entries: RawList {
                ptr: std::ptr::null_mut(),
                len: 0,
                cap: 0,
            },
            table: std::ptr::null_mut(),
            table_cap: 0,
        }
    }

    extern "C" fn eq_u64(a: *const u8, b: *const u8) -> u8 {
        // SAFETY: the tests only pass `u64` keys.
        u8::from(unsafe { a.cast::<u64>().read_unaligned() == b.cast::<u64>().read_unaligned() })
    }

    /// Entries of the test map: hash, then a `u64` key.
    const ENTRY: usize = 16;

    fn insert(map: &mut RawMap, key: u64) {
        // A poor hash, to exercise collisions.
        let hash = key % 3;
        // SAFETY: the map holds such entries; the key is not in it.
        unsafe {
            let entry = pika_map_push(map, hash, ENTRY, 8);
            entry.add(8).cast::<u64>().write_unaligned(key);
        }
    }

    fn find(map: &RawMap, key: u64) -> i64 {
        // SAFETY: the map holds such entries.
        unsafe { pika_map_find(map, key % 3, (&raw const key).cast(), eq_u64, 8, ENTRY) }
    }

    #[test]
    fn lists_grow_open_and_close() {
        let _guard = COUNTER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = live_allocations();
        let mut list = RawList {
            ptr: std::ptr::null_mut(),
            len: 0,
            cap: 0,
        };
        // SAFETY: the list holds `u32` elements.
        unsafe {
            for value in 0..10u32 {
                let slot = pika_list_open(&raw mut list, list.len, 4, 4);
                slot.cast::<u32>().write_unaligned(value);
            }
            let slot = pika_list_open(&raw mut list, 0, 4, 4);
            slot.cast::<u32>().write_unaligned(99);
            pika_list_close(&raw mut list, 3, 4);
            let values: Vec<u32> = (0..list.len)
                .map(|i| list.element(i, 4).cast::<u32>().read_unaligned())
                .collect();
            assert_eq!(values, [99, 0, 1, 3, 4, 5, 6, 7, 8, 9]);
            assert_eq!(live_allocations(), before + 1);
            pika_list_free(&raw mut list, 4, 4);
        }
        assert_eq!(live_allocations(), before);
    }

    #[test]
    fn maps_find_insert_and_remove_in_order() {
        let _guard = COUNTER
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let before = live_allocations();
        let mut map = empty_map();
        assert_eq!(find(&map, 1), -1);
        for key in 0..100 {
            insert(&mut map, key);
        }
        for key in 0..100 {
            assert_eq!(find(&map, key), i64::try_from(key).expect("small"));
        }
        assert_eq!(find(&map, 100), -1);
        // SAFETY: the map holds such entries; entry 10 has nothing to destroy.
        unsafe { pika_map_remove(&raw mut map, 10, ENTRY) };
        assert_eq!(find(&map, 10), -1);
        assert_eq!(find(&map, 11), 10);
        assert_eq!(find(&map, 99), 98);
        // SAFETY: as above.
        unsafe {
            pika_map_clear(&raw mut map);
            assert_eq!(find(&map, 5), -1);
            pika_map_free(&raw mut map, ENTRY, 8);
        }
        assert_eq!(live_allocations(), before);
    }

    #[test]
    fn hashes_of_bytes_differ() {
        // SAFETY: the slices are readable.
        let (a, b) = unsafe {
            (
                pika_hash_bytes(b"ab".as_ptr(), 2),
                pika_hash_bytes(b"ba".as_ptr(), 2),
            )
        };
        assert_ne!(a, b);
    }
}
