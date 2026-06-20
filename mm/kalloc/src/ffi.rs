// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! C-compatible allocation shims for the kernel allocator.
#![allow(unsafe_op_in_unsafe_fn)]

use core::{
    alloc::Layout,
    ffi::{c_int, c_void},
    mem::size_of,
    ptr::{self, NonNull},
};

use crate::global_allocator;

fn create_layout(user_size: usize) -> Option<Layout> {
    let metadata_size = size_of::<usize>();
    let total_size = user_size + metadata_size;

    Layout::from_size_align(total_size, size_of::<usize>()).ok()
}

#[unsafe(no_mangle)]
/// C `malloc` shim backed by the kernel global allocator.
///
/// # Safety
///
/// This symbol follows the C allocator ABI. Callers must treat the returned
/// pointer exactly like a `malloc` allocation: pass it back only to this
/// module's `free`, ensure the requested `size` matches the intended object
/// layout, and never dereference a null return value.
pub unsafe extern "C" fn malloc(size: c_int) -> *mut c_void {
    if size <= 0 {
        return ptr::null_mut();
    }

    let user_size = size as usize;
    if let Some(layout) = create_layout(user_size) {
        match global_allocator().alloc(layout) {
            // SAFETY: `ptr` is a live allocation of at least `layout.size()`
            // bytes. The first word stores the user size metadata, and the
            // returned pointer advances exactly past that metadata header.
            Ok(ptr) => unsafe {
                *(ptr.as_ptr() as *mut usize) = user_size;
                ptr.as_ptr().add(size_of::<usize>()) as *mut c_void
            },
            Err(_) => ptr::null_mut(),
        }
    } else {
        ptr::null_mut()
    }
}

#[unsafe(no_mangle)]
/// C `free` shim paired with [`malloc`] and [`calloc`].
///
/// # Safety
///
/// `ptr` must be null or a live allocation previously returned by this
/// module's `malloc`/`calloc`, and it must not have been freed already.
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }

    let metadata_size = size_of::<usize>();
    // SAFETY: `ptr` was previously returned by `malloc`/`calloc`, so it points
    // just past the metadata header and can be rewound by that fixed size.
    let base_ptr = unsafe { (ptr as *mut u8).sub(metadata_size) };

    // SAFETY: `base_ptr` now points at the metadata word written by `malloc`.
    let user_size = unsafe { *(base_ptr as *const usize) };
    let total_size = user_size + metadata_size;

    // SAFETY: the allocation was created with the same size/alignment pair in
    // `malloc`, and `base_ptr` is the original pointer returned by allocator.
    unsafe {
        let layout = Layout::from_size_align_unchecked(total_size, size_of::<usize>());
        global_allocator().dealloc(NonNull::new_unchecked(base_ptr), layout);
    }
}

#[unsafe(no_mangle)]
/// C `calloc` shim backed by the kernel global allocator.
///
/// # Safety
///
/// The same pointer-ownership rules as [`malloc`] apply. Callers must ensure
/// `nmemb * size` describes the intended object region and use the returned
/// pointer only if it is non-null.
pub unsafe extern "C" fn calloc(nmemb: c_int, size: c_int) -> *mut c_void {
    let total_size = nmemb.saturating_mul(size);
    if total_size == 0 {
        return ptr::null_mut();
    }

    // SAFETY: `malloc` follows the same allocation contract as this module's
    // exported C ABI; `total_size > 0` was checked above.
    let ptr = unsafe { malloc(total_size) };
    if !ptr.is_null() {
        // SAFETY: `ptr` points to `total_size` writable bytes returned by
        // `malloc`, so zero-initializing that region is valid.
        unsafe {
            ptr::write_bytes(ptr as *mut u8, 0, total_size as usize);
        }
    }
    ptr
}

#[unsafe(no_mangle)]
/// Bounds-checked C `memcpy` helper.
///
/// # Safety
///
/// `dest` and `src` must each denote valid regions of at least `len` bytes.
/// The two regions must not overlap. `dest_len` must describe the full size of
/// the destination object so the bounds check is meaningful.
pub unsafe extern "C" fn __memcpy_chk(
    dest: *mut c_void,
    src: *const c_void,
    len: c_int,
    dest_len: c_int,
) -> *mut c_void {
    if dest.is_null() || src.is_null() {
        return dest;
    }

    if len > dest_len {
        return ptr::null_mut();
    }

    // SAFETY: the null and destination-size checks above establish valid,
    // non-overlapping byte ranges of `len` bytes for the copy.
    unsafe {
        ptr::copy_nonoverlapping(src as *const u8, dest as *mut u8, len as usize);
    }
    dest
}

#[unsafe(no_mangle)]
/// C `memcpy` shim.
///
/// # Safety
///
/// `dest` and `src` must each denote valid regions of at least `len` bytes,
/// and the two regions must not overlap.
pub unsafe extern "C" fn memcpy(dest: *mut c_void, src: *const c_void, len: usize) -> *mut c_void {
    if dest.is_null() || src.is_null() {
        return dest;
    }

    let mut index = 0;
    while index < len {
        // SAFETY: the C ABI contract for `memcpy` requires valid
        // non-overlapping `len`-byte regions at `src` and `dest`; the loop
        // copies exactly one in-bounds byte at `index`.
        unsafe {
            *(dest as *mut u8).add(index) = *(src as *const u8).add(index);
        }
        index += 1;
    }
    dest
}

#[unsafe(no_mangle)]
/// C `memmove` shim.
///
/// # Safety
///
/// `dest` and `src` must each denote valid regions of at least `len` bytes.
/// Unlike [`memcpy`], the regions may overlap.
pub unsafe extern "C" fn memmove(dest: *mut c_void, src: *const c_void, len: usize) -> *mut c_void {
    if dest.is_null() || src.is_null() || core::ptr::eq(dest, src.cast_mut()) {
        return dest;
    }

    let dest_ptr = dest as *mut u8;
    let src_ptr = src as *const u8;
    if (dest_ptr as usize) < (src_ptr as usize) || (dest_ptr as usize) >= (src_ptr as usize + len) {
        let mut index = 0;
        while index < len {
            // SAFETY: the C ABI contract for `memmove` requires valid
            // `len`-byte regions at `src` and `dest`; this forward copy path
            // is correct when the regions do not overlap backward.
            unsafe {
                *dest_ptr.add(index) = *src_ptr.add(index);
            }
            index += 1;
        }
    } else {
        let mut index = len;
        while index != 0 {
            index -= 1;
            // SAFETY: the C ABI contract for `memmove` requires valid
            // `len`-byte regions at `src` and `dest`; this backward copy path
            // preserves bytes when the regions overlap.
            unsafe {
                *dest_ptr.add(index) = *src_ptr.add(index);
            }
        }
    }
    dest
}

#[unsafe(no_mangle)]
/// C `memset` shim.
///
/// # Safety
///
/// `dest` must denote a valid writable region of at least `len` bytes.
pub unsafe extern "C" fn memset(dest: *mut c_void, value: c_int, len: usize) -> *mut c_void {
    if dest.is_null() {
        return dest;
    }

    let byte = value as u8;
    let mut index = 0;
    while index < len {
        // SAFETY: the C ABI contract for `memset` requires a valid writable
        // `len`-byte destination range; the loop writes exactly one in-bounds
        // byte at `index`.
        unsafe {
            *(dest as *mut u8).add(index) = byte;
        }
        index += 1;
    }
    dest
}

#[unsafe(no_mangle)]
/// C `memcmp` shim.
///
/// # Safety
///
/// `lhs` and `rhs` must each denote valid readable regions of at least `len`
/// bytes.
pub unsafe extern "C" fn memcmp(lhs: *const c_void, rhs: *const c_void, len: usize) -> c_int {
    if core::ptr::eq(lhs, rhs) || len == 0 {
        return 0;
    }

    let mut index = 0;
    while index < len {
        // SAFETY: the caller's C ABI contract provides valid `len`-byte input
        // ranges, and the loop only reads the current byte from each.
        let left = unsafe { *(lhs as *const u8).add(index) };
        // SAFETY: the caller's C ABI contract provides valid `len`-byte input
        // ranges, and the loop only reads the current byte from each.
        let right = unsafe { *(rhs as *const u8).add(index) };
        if left != right {
            return left as c_int - right as c_int;
        }
        index += 1;
    }
    0
}
