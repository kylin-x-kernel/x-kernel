// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User memory helpers and user pointer wrappers.

use alloc::string::String;
use core::{
    ffi::c_char,
    mem::{MaybeUninit, transmute},
};

use kerrno::{KError, KResult};
use kio::prelude::*;
use osvm::{load_vec, load_vec_until_null, read_vm_mem, write_vm_mem};

#[macro_export]
macro_rules! nullable {
    ($ptr:ident.$func:ident($($arg:expr),*)) => {
        if $ptr.is_null() {
            Ok(None)
        } else {
            Some($ptr.$func($($arg),*)).transpose()
        }
    };
}

/// Load a null-terminated string from user virtual memory
pub fn vm_load_string(ptr: *const c_char) -> KResult<String> {
    #[allow(clippy::unnecessary_cast)]
    let bytes = load_vec_until_null(ptr as *const u8)?;
    String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
}

/// Load a string with specified length from user virtual memory
pub fn vm_load_string_with_len(ptr: *const c_char, len: usize) -> KResult<String> {
    #[allow(clippy::unnecessary_cast)]
    let bytes = load_vec(ptr as *const u8, len)?;
    String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
}

/// A read-only buffer in the VM's memory.
///
/// It implements the `kio::Read` trait, allowing it to be used with other I/O
/// operations.
pub struct VmBytes {
    /// The pointer to the start of the buffer in the VM's memory.
    pub ptr: *const u8,
    /// The length of the buffer.
    pub len: usize,
}

impl VmBytes {
    /// Creates a new `VmBytes` from a raw pointer and a length.
    pub fn new(ptr: *const u8, len: usize) -> Self {
        Self { ptr, len }
    }

    /// Cast the `VmBytes` to a mutable `VmBytesMut`
    pub fn cast_mut(&self) -> VmBytesMut {
        VmBytesMut::new(self.ptr as *mut u8, self.len)
    }
}

impl Read for VmBytes {
    /// Reads bytes from the VM's memory into the provided buffer.
    fn read(&mut self, buf: &mut [u8]) -> kio::Result<usize> {
        let len = self.len.min(buf.len());
        read_vm_mem(self.ptr, unsafe {
            transmute::<&mut [u8], &mut [MaybeUninit<u8>]>(&mut buf[..len])
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

/// A mutable buffer in the VM's memory.
///
/// It implements the `kio::Write` trait, allowing it to be used with other I/O
/// operations.
pub struct VmBytesMut {
    /// The pointer to the start of the buffer in the VM's memory.
    pub ptr: *mut u8,
    /// The length of the buffer.
    pub len: usize,
}

impl VmBytesMut {
    /// Creates a new `VmBytesMut` from a raw pointer and a length.
    pub fn new(ptr: *mut u8, len: usize) -> Self {
        Self { ptr, len }
    }

    /// Cast the `VmBytesMut` to a read-only `VmBytes`
    pub fn cast_const(&self) -> VmBytes {
        VmBytes::new(self.ptr, self.len)
    }
}

impl Write for VmBytesMut {
    /// Writes bytes from the provided buffer into the VM's memory.
    fn write(&mut self, buf: &[u8]) -> kio::Result<usize> {
        let len = self.len.min(buf.len());
        write_vm_mem(self.ptr, &buf[..len])?;
        self.ptr = self.ptr.wrapping_add(len);
        self.len -= len;
        Ok(len)
    }

    /// Flushes the buffer. This is a no-op for `VmBytesMut`.
    fn flush(&mut self) -> kio::Result {
        Ok(())
    }
}

impl IoBufMut for VmBytesMut {
    fn remaining_mut(&self) -> usize {
        self.len
    }
}
