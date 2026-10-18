//! The extension's global allocator: the system allocator, reporting the
//! large blocks it has outstanding to the engine (`zipp_vm::note_large_block`).
//!
//! Part of the engine's collection schedule counts bytes, and a payload that
//! grows in place (an array pushed to its size, a filling Map) is visible to
//! it only through such reports. Without them a loop that builds and drops
//! 16 KB arrays held 320 MB before the collector ran; with them, 14 MB, at no
//! measurable cost. The `zipp` CLI installs the same kind of wrapper.
//!
//! The allocator governs this extension's Rust allocations only (the engine's,
//! and the binding's own), not Ruby's. Blocks under `LARGE_BLOCK_BYTES` cost
//! one compare.
//!
//! `GlobalAlloc` is an unsafe trait, so this is the one place the engine's
//! `forbid(unsafe_code)` does not reach: the engine stays safe code, and this
//! wrapper only forwards to `System`.

use std::alloc::{GlobalAlloc, Layout, System};

use zipp_vm::{note_large_block, LARGE_BLOCK_BYTES};

struct ReportingAlloc;

#[global_allocator]
static GLOBAL: ReportingAlloc = ReportingAlloc;

// SAFETY: every method forwards its arguments unchanged to the same method of
// `System` and returns its result, so the blocks handed out are `System`'s and
// meet `GlobalAlloc`'s requirements as `System`'s do; a block is always freed
// or resized by `System`, which allocated it. The only other work is
// `note_large_block`, which neither allocates (no re-entry into this
// allocator), nor takes a lock, nor touches thread-local storage, nor panics
// (so nothing unwinds out of the allocator): it is relaxed atomic loads,
// stores and adds on statics, whose sums are bounded by the bytes
// outstanding. A failed allocation (null) is not reported, and
// a failed `realloc` leaves the old block, still counted, in place.
unsafe impl GlobalAlloc for ReportingAlloc {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        // SAFETY: the caller's guarantees for `layout` (non-zero size) are
        // exactly `System.alloc`'s preconditions.
        let p = unsafe { System.alloc(layout) };
        if layout.size() >= LARGE_BLOCK_BYTES && !p.is_null() {
            note_large_block(0, layout.size());
        }
        p
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        // SAFETY: as in `alloc`.
        let p = unsafe { System.alloc_zeroed(layout) };
        if layout.size() >= LARGE_BLOCK_BYTES && !p.is_null() {
            note_large_block(0, layout.size());
        }
        p
    }

    #[inline]
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        if layout.size() >= LARGE_BLOCK_BYTES {
            note_large_block(layout.size(), 0);
        }
        // SAFETY: the caller guarantees `ptr` came from this allocator with
        // `layout`, and every block this allocator hands out is `System`'s,
        // allocated with the same layout.
        unsafe { System.dealloc(ptr, layout) }
    }

    #[inline]
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        // SAFETY: as in `dealloc` for `ptr` and `layout`; the caller's
        // guarantees for `new_size` are `System.realloc`'s.
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if (layout.size() >= LARGE_BLOCK_BYTES || new_size >= LARGE_BLOCK_BYTES) && !p.is_null() {
            note_large_block(layout.size(), new_size);
        }
        p
    }
}
