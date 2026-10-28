// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Helpers for reading/writing user virtual memory.
//!
//! This crate provides checked, typed access to another address space's
//! memory from kernel code: byte-level helpers ([`read_vm_mem`],
//! [`write_vm_mem`], [`read_vm_bytes`], [`write_vm_bytes`]), pointer traits
//! ([`VirtPtr`], [`VirtMutPtr`]), streaming I/O adapters ([`VmBytes`],
//! [`VmBytesMut`]), and, with the `alloc` feature, heap-loading helpers
//! (`load_vec`, `load_vec_until_null`).
//!
//! The actual fault-checked copy primitive is supplied out-of-tree through
//! the [`VirtMemIo`] provider contract (implemented by `kuaccess`), so this
//! crate stays free of architecture-specific fault handling.
#![no_std]
#![feature(maybe_uninit_as_bytes)]

use core::{mem::MaybeUninit, slice};

use extern_trait::extern_trait;
use kerrno::KError;

/// Errors returned by virtual memory access helpers.
#[derive(Debug, PartialEq, Clone, Copy)]
pub enum MemError {
    /// The address failed alignment or range checks before any copy was
    /// attempted (maps to `EFAULT`).
    InvalidAddr,
    /// The provider rejected or faulted on the access; the destination
    /// buffer contents are unspecified (maps to `EFAULT`).
    NoAccess,
    /// The provider ran out of memory while resolving the access (maps to
    /// `ENOMEM`).
    OutOfMemory,
    /// A length-limited load exceeded its fixed cap (`alloc` feature only;
    /// maps to `ENAMETOOLONG`).
    #[cfg(feature = "alloc")]
    NameTooLong,
}

impl From<MemError> for KError {
    fn from(e: MemError) -> Self {
        match e {
            MemError::InvalidAddr | MemError::NoAccess => KError::BadAddress,
            MemError::OutOfMemory => KError::NoMemory,
            #[cfg(feature = "alloc")]
            MemError::NameTooLong => KError::NameTooLong,
        }
    }
}

/// Result type for virtual memory operations.
pub type MemResult<T = ()> = Result<T, MemError>;

/// External trait that supplies platform-specific memory I/O.
///
/// # Safety
///
/// Implementers must ensure `read_mem` and `write_mem` only succeed when the
/// supplied virtual range is accessible for the requested operation, and that
/// successful reads fully initialize `out`. Implementations must not create
/// typed references to user memory; they should perform checked byte copies.
#[extern_trait(MemImpl)]
pub unsafe trait VirtMemIo: 'static {
    /// Creates a provider instance.
    ///
    /// Implementations must be cheap and infallible; state, if any, is
    /// per-call scratch (e.g. a fault-entry guard), not a resource handle.
    fn new() -> Self;
    /// Copies `out.len()` bytes from `addr` into `out`.
    ///
    /// On success every byte of `out` is initialized; on failure `out`'s
    /// contents are unspecified.
    ///
    /// # Errors
    ///
    /// Returns [`MemError::NoAccess`] when any byte of the range is not
    /// readable through the provider's fault-checked path.
    fn read_mem(&mut self, addr: usize, out: &mut [MaybeUninit<u8>]) -> MemResult;
    /// Copies `src` to the range starting at `addr`.
    ///
    /// Partial writes must not be reported as success; the provider either
    /// copies the whole slice or fails.
    ///
    /// # Errors
    ///
    /// Returns [`MemError::NoAccess`] when any byte of the range is not
    /// writable through the provider's fault-checked path.
    fn write_mem(&mut self, addr: usize, src: &[u8]) -> MemResult;
}

/// Reads `out.len()` elements from virtual memory into an uninitialized
/// buffer.
///
/// `p` must be aligned for `T`; the copy itself is byte-based and imposes no
/// `T` validity requirement.
///
/// # Errors
///
/// Returns [`MemError::InvalidAddr`] when `p` is misaligned, or the
/// provider's error when the range is inaccessible.
pub fn read_vm_mem<T>(p: *const T, out: &mut [MaybeUninit<T>]) -> MemResult {
    if !p.is_aligned() {
        return Err(MemError::InvalidAddr);
    }
    MemImpl::new().read_mem(p.addr(), out.as_bytes_mut())
}

/// Reads raw bytes from virtual memory without imposing typed alignment.
///
/// # Errors
///
/// Returns the provider's error when the range is inaccessible.
pub fn read_vm_bytes(p: *const u8, out: &mut [MaybeUninit<u8>]) -> MemResult {
    MemImpl::new().read_mem(p.addr(), out)
}

/// Writes a typed slice to virtual memory.
///
/// # Errors
///
/// Returns [`MemError::InvalidAddr`] when `p` is misaligned, or the
/// provider's error when the range is inaccessible.
pub fn write_vm_mem<T>(p: *mut T, src: &[T]) -> MemResult {
    if !p.is_aligned() {
        return Err(MemError::InvalidAddr);
    }
    // SAFETY: `src` is a live slice, so viewing its contiguous storage as bytes
    // for the same length is layout-preserving.
    let bytes = unsafe { slice::from_raw_parts(src.as_ptr().cast::<u8>(), size_of_val(src)) };
    MemImpl::new().write_mem(p.addr(), bytes)
}

/// Writes raw bytes to virtual memory without imposing typed alignment.
///
/// # Errors
///
/// Returns the provider's error when the range is inaccessible.
pub fn write_vm_bytes(p: *mut u8, src: &[u8]) -> MemResult {
    MemImpl::new().write_mem(p.addr(), src)
}

mod ptrs;
mod vm_io;

pub use ptrs::{VirtMutPtr, VirtPtr};
pub use vm_io::{VmBytes, VmBytesMut};

#[cfg(feature = "alloc")]
mod heap;
#[cfg(feature = "alloc")]
pub use heap::{load_vec, load_vec_unsafe, load_vec_until_null};
