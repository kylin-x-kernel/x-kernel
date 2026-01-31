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
pub unsafe extern "C" fn malloc(size: c_int) -> *mut c_void {
    if size <= 0 {
        return ptr::null_mut();
    }

    let user_size = size as usize;
    if let Some(layout) = create_layout(user_size) {
        match global_allocator().alloc(layout) {
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
pub unsafe extern "C" fn free(ptr: *mut c_void) {
    if ptr.is_null() {
        return;
    }

    let metadata_size = size_of::<usize>();
    let base_ptr = unsafe { (ptr as *mut u8).sub(metadata_size) };

    let user_size = unsafe { *(base_ptr as *const usize) };
    let total_size = user_size + metadata_size;

    unsafe {
        let layout = Layout::from_size_align_unchecked(total_size, size_of::<usize>());
        global_allocator().dealloc(NonNull::new_unchecked(base_ptr), layout);
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn calloc(nmemb: c_int, size: c_int) -> *mut c_void {
    let total_size = nmemb.saturating_mul(size);
    if total_size == 0 {
        return ptr::null_mut();
    }

    let ptr = unsafe { malloc(total_size) };
    if !ptr.is_null() {
        unsafe {
            ptr::write_bytes(ptr as *mut u8, 0, total_size as usize);
        }
    }
    ptr
}

#[unsafe(no_mangle)]
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

    unsafe {
        ptr::copy_nonoverlapping(src as *const u8, dest as *mut u8, len as usize);
    }
    dest
}
