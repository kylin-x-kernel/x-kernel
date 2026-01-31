// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Helpers for reading/writing user virtual memory.
#![no_std]
#![feature(maybe_uninit_as_bytes)]
#![allow(clippy::missing_safety_doc)]

use core::{mem::MaybeUninit, slice};

use extern_trait::extern_trait;
use kerrno::KError;

/// Errors returned by virtual memory access helpers.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MemError {
    InvalidAddr,
    NoAccess,
    #[cfg(feature = "alloc")]
    NameTooLong,
}

impl From<MemError> for KError {
    fn from(e: MemError) -> Self {
        match e {
            MemError::InvalidAddr | MemError::NoAccess => KError::BadAddress,
            #[cfg(feature = "alloc")]
            MemError::NameTooLong => KError::NameTooLong,
        }
    }
}

/// Result type for virtual memory operations.
pub type MemResult<T = ()> = Result<T, MemError>;

/// External trait that supplies platform-specific memory I/O.
#[extern_trait(MemImpl)]
pub unsafe trait VirtMemIo: 'static {
    fn new() -> Self;
    fn read_mem(&mut self, addr: usize, out: &mut [MaybeUninit<u8>]) -> MemResult;
    fn write_mem(&mut self, addr: usize, src: &[u8]) -> MemResult;
}

/// Read virtual memory into an uninitialized buffer.
pub fn read_vm_mem<T>(p: *const T, out: &mut [MaybeUninit<T>]) -> MemResult {
    if !p.is_aligned() {
        return Err(MemError::InvalidAddr);
    }
    MemImpl::new().read_mem(p.addr(), out.as_bytes_mut())
}

/// Write a typed slice to virtual memory.
pub fn write_vm_mem<T>(p: *mut T, src: &[T]) -> MemResult {
    if !p.is_aligned() {
        return Err(MemError::InvalidAddr);
    }
    let bytes = unsafe { slice::from_raw_parts(src.as_ptr().cast::<u8>(), size_of_val(src)) };
    MemImpl::new().write_mem(p.addr(), bytes)
}

mod ptrs;
pub use ptrs::{VirtMutPtr, VirtPtr};

#[cfg(feature = "alloc")]
mod heap;
#[cfg(feature = "alloc")]
pub use heap::{load_vec, load_vec_unsafe, load_vec_until_null};
