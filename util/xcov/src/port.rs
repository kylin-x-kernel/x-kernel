// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform portability layer.
//!
//! Provides atomic operations, memory operations, and custom allocators
//! without libc dependency.

use core::ptr;

pub use portable_atomic::{AtomicU32, AtomicU64, AtomicUsize};

/// Atomic compare-and-swap on a 64-bit value.
pub fn bool_cmpxchg_u64(ptr: *mut u64, old_val: u64, new_val: u64) -> bool {
    unsafe {
        let a = &*(ptr as *const AtomicU64);
        a.compare_exchange(
            old_val,
            new_val,
            core::sync::atomic::Ordering::SeqCst,
            core::sync::atomic::Ordering::SeqCst,
        )
        .is_ok()
    }
}

/// Zero a memory region.
///
/// # Safety
///
/// Caller must guarantee `[ptr, ptr + len)` is valid for writes.
pub unsafe fn mem_zero(ptr: *mut u8, len: usize) {
    unsafe { ptr::write_bytes(ptr, 0, len) }
}

/// Fill a memory region with a byte value.
///
/// # Safety
///
/// Caller must guarantee `[ptr, ptr + len)` is valid for writes.
pub unsafe fn mem_set(ptr: *mut u8, val: u8, len: usize) {
    unsafe { ptr::write_bytes(ptr, val, len) }
}

/// Compare two memory regions.
///
/// # Safety
///
/// Caller must guarantee `[a, a + len)` and `[b, b + len)` are valid for reads.
pub unsafe fn mem_cmp(a: *const u8, b: *const u8, len: usize) -> i32 {
    unsafe {
        let mut offset = 0;
        while offset < len {
            let va = *a.add(offset);
            let vb = *b.add(offset);
            if va != vb {
                return va as i32 - vb as i32;
            }
            offset += 1;
        }
        0
    }
}

/// Copy memory (non-overlapping).
///
/// # Safety
///
/// Caller must guarantee `[dst, dst + len)` is valid for writes
/// and `[src, src + len)` is valid for reads, with no overlap.
pub unsafe fn mem_copy(dst: *mut u8, src: *const u8, len: usize) {
    unsafe { ptr::copy_nonoverlapping(src, dst, len) }
}

/// Allocate zeroed memory.
///
/// # Safety
///
/// Caller must guarantee `align` is a valid alignment (power of two).
#[cfg(feature = "alloc")]
pub unsafe fn alloc_zeroed(size: usize, align: usize) -> *mut u8 {
    unsafe {
        extern crate alloc;
        use alloc::alloc::{Layout, alloc_zeroed};
        let layout = Layout::from_size_align(size, align).unwrap();
        alloc_zeroed(layout)
    }
}

/// Deallocate memory.
///
/// # Safety
///
/// Caller must guarantee `ptr` was returned by `alloc_zeroed` with the same
/// `size` and `align`.
#[cfg(feature = "alloc")]
pub unsafe fn dealloc(ptr: *mut u8, size: usize, align: usize) {
    unsafe {
        extern crate alloc;
        use alloc::alloc::{Layout, dealloc};
        let layout = Layout::from_size_align(size, align).unwrap();
        dealloc(ptr, layout)
    }
}

/// Page size stub. Continuous mode is not fully supported.
pub const fn get_page_size() -> usize {
    1
}

/// Returns the number of padding bytes to align `offset` to page boundary.
pub fn calculate_bytes_needed_to_page_align(offset: u64, page_size: u64) -> u64 {
    let offset_mod_page = offset % page_size;
    if offset_mod_page > 0 {
        page_size - offset_mod_page
    } else {
        0
    }
}

/// Counter entry size based on profile version.
pub fn counter_entry_size(version: u64) -> usize {
    if version & crate::types::VARIANT_MASK_BYTE_COVERAGE != 0 {
        1
    } else {
        8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mem_zero_clears_buffer() {
        let mut buf = [0xFFu8; 16];
        unsafe { mem_zero(buf.as_mut_ptr(), buf.len()) };
        assert!(buf.iter().all(|&b| b == 0));
    }

    #[test]
    fn mem_set_fills_buffer() {
        let mut buf = [0u8; 8];
        unsafe { mem_set(buf.as_mut_ptr(), 0xAB, buf.len()) };
        assert!(buf.iter().all(|&b| b == 0xAB));
    }

    #[test]
    fn mem_cmp_equal_buffers() {
        let a = [1u8, 2, 3, 4];
        let b = [1u8, 2, 3, 4];
        assert_eq!(unsafe { mem_cmp(a.as_ptr(), b.as_ptr(), 4) }, 0);
    }

    #[test]
    fn mem_cmp_different_buffers() {
        let a = [1u8, 2, 3, 4];
        let b = [1u8, 2, 0, 4];
        assert!(unsafe { mem_cmp(a.as_ptr(), b.as_ptr(), 4) } > 0);
    }

    #[test]
    fn mem_copy_copies_bytes() {
        let src = [10u8, 20, 30];
        let mut dst = [0u8; 3];
        unsafe { mem_copy(dst.as_mut_ptr(), src.as_ptr(), 3) };
        assert_eq!(dst, src);
    }

    #[test]
    fn page_align_padding() {
        assert_eq!(calculate_bytes_needed_to_page_align(0, 4096), 0);
        assert_eq!(calculate_bytes_needed_to_page_align(100, 4096), 3996);
        assert_eq!(calculate_bytes_needed_to_page_align(4096, 4096), 0);
    }

    #[cfg(feature = "alloc")]
    #[test]
    fn alloc_and_dealloc() {
        let ptr = unsafe { alloc_zeroed(64, 8) };
        assert!(!ptr.is_null());
        unsafe { dealloc(ptr, 64, 8) };
    }
}
