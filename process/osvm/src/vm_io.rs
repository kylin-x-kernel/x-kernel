// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Streaming I/O adapters for user-space virtual memory.

use core::mem::{self, MaybeUninit};

use kio::prelude::*;

use crate::{read_vm_mem, write_vm_mem};

/// A read-only buffer in user-space virtual memory.
///
/// Implements [`Read`] so it can be passed to socket/file send operations.
pub struct VmBytes {
    /// Pointer to the start of the buffer.
    pub ptr: *const u8,
    /// Remaining length of the buffer.
    pub len: usize,
}

impl VmBytes {
    /// Creates a new `VmBytes` from a raw pointer and a length.
    pub fn new(ptr: *const u8, len: usize) -> Self {
        Self { ptr, len }
    }

    /// Cast to a mutable [`VmBytesMut`].
    pub fn cast_mut(&self) -> VmBytesMut {
        VmBytesMut::new(self.ptr as *mut u8, self.len)
    }
}

impl Read for VmBytes {
    fn read(&mut self, buf: &mut [u8]) -> kio::Result<usize> {
        let len = self.len.min(buf.len());
        read_vm_mem(self.ptr, unsafe {
            mem::transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut buf[..len])
        })?;
        self.ptr = self.ptr.wrapping_add(len);
        self.len -= len;
        Ok(len)
    }
}

impl IoBuf for VmBytes {
    fn remaining(&self) -> usize {
        self.len
    }
}

/// A mutable buffer in user-space virtual memory.
///
/// Implements [`Write`] so it can be passed to socket/file receive operations.
pub struct VmBytesMut {
    /// Pointer to the start of the buffer.
    pub ptr: *mut u8,
    /// Remaining length of the buffer.
    pub len: usize,
}

impl VmBytesMut {
    /// Creates a new `VmBytesMut` from a raw pointer and a length.
    pub fn new(ptr: *mut u8, len: usize) -> Self {
        Self { ptr, len }
    }

    /// Cast to a read-only [`VmBytes`].
    pub fn cast_const(&self) -> VmBytes {
        VmBytes::new(self.ptr, self.len)
    }
}

impl Write for VmBytesMut {
    fn write(&mut self, buf: &[u8]) -> kio::Result<usize> {
        let len = self.len.min(buf.len());
        write_vm_mem(self.ptr, &buf[..len])?;
        self.ptr = self.ptr.wrapping_add(len);
        self.len -= len;
        Ok(len)
    }

    fn flush(&mut self) -> kio::Result {
        Ok(())
    }
}

impl IoBufMut for VmBytesMut {
    fn remaining_mut(&self) -> usize {
        self.len
    }
}
