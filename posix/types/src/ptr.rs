// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! User-space pointer wrappers for POSIX syscalls.
//!
//! Provides [`UserPtr`] and [`UserConstPtr`] that wrap raw user-space pointers
//! with copy-from-user / copy-to-user helpers for syscall ABI values.

extern crate alloc;

use alloc::{string::String, vec::Vec};
use core::{
    ffi::c_char,
    mem::{MaybeUninit, size_of, size_of_val},
    ptr, slice,
};

use bytemuck::AnyBitPattern;
use kerrno::{KError, KResult};
use osvm::{VirtMutPtr, VirtPtr, load_vec_until_null, read_vm_bytes, write_vm_bytes};

fn as_uninit_bytes<T>(value: &mut MaybeUninit<T>) -> &mut [MaybeUninit<u8>] {
    unsafe {
        slice::from_raw_parts_mut(value.as_mut_ptr().cast::<MaybeUninit<u8>>(), size_of::<T>())
    }
}

fn spare_as_uninit_bytes<T>(spare: &mut [MaybeUninit<T>]) -> &mut [MaybeUninit<u8>] {
    unsafe {
        slice::from_raw_parts_mut(
            spare.as_mut_ptr().cast::<MaybeUninit<u8>>(),
            size_of_val(spare),
        )
    }
}

/// A mutable pointer to user-space memory.
///
/// Provides copy-to-user helpers for syscall ABI values.
#[repr(transparent)]
pub struct UserPtr<T>(*mut T);

impl<T> Clone for UserPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for UserPtr<T> {}

impl<T> PartialEq for UserPtr<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T> From<usize> for UserPtr<T> {
    fn from(value: usize) -> Self {
        UserPtr(value as *mut T)
    }
}

impl<T> From<*mut T> for UserPtr<T> {
    fn from(value: *mut T) -> Self {
        UserPtr(value)
    }
}

impl<T> Default for UserPtr<T> {
    fn default() -> Self {
        Self(ptr::null_mut())
    }
}

impl<T> UserPtr<T> {
    /// Returns `true` if the pointer is null.
    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    /// Returns `None` if the pointer is null, `Some(self)` otherwise.
    pub fn check_non_null(&self) -> Option<Self> {
        if self.0.is_null() { None } else { Some(*self) }
    }

    /// Cast to a `UserPtr` of a different type.
    pub fn cast<U>(self) -> UserPtr<U> {
        UserPtr(self.0 as *mut U)
    }

    /// Write a value to user memory with copy-to-user semantics.
    pub fn write_vm(self, value: T) -> osvm::MemResult {
        self.write_vm_slice(slice::from_ref(&value))
    }

    /// Write a slice of values to user memory.
    pub fn write_vm_slice(self, data: &[T]) -> osvm::MemResult {
        let bytes = unsafe { slice::from_raw_parts(data.as_ptr().cast::<u8>(), size_of_val(data)) };
        write_vm_bytes(self.0.cast::<u8>(), bytes)
    }
}

impl<T> VirtPtr for UserPtr<T> {
    type Target = T;

    fn as_ptr(self) -> *const Self::Target {
        self.0
    }
}

impl<T> VirtMutPtr for UserPtr<T> {}

/// A read-only pointer to user-space memory.
///
/// Provides copy-from-user helpers for syscall ABI values.
#[repr(transparent)]
pub struct UserConstPtr<T>(*const T);

impl<T> Clone for UserConstPtr<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T> Copy for UserConstPtr<T> {}

impl<T> PartialEq for UserConstPtr<T> {
    fn eq(&self, other: &Self) -> bool {
        self.0 == other.0
    }
}

impl<T> From<usize> for UserConstPtr<T> {
    fn from(value: usize) -> Self {
        UserConstPtr(value as *const T)
    }
}

impl<T> From<*const T> for UserConstPtr<T> {
    fn from(value: *const T) -> Self {
        UserConstPtr(value)
    }
}

impl<T> Default for UserConstPtr<T> {
    fn default() -> Self {
        Self(ptr::null())
    }
}

impl<T> UserConstPtr<T> {
    /// Returns `true` if the pointer is null.
    pub fn is_null(&self) -> bool {
        self.0.is_null()
    }

    /// Returns `None` if the pointer is null, `Some(self)` otherwise.
    pub fn check_non_null(&self) -> Option<Self> {
        if self.0.is_null() { None } else { Some(*self) }
    }

    /// Cast to a `UserConstPtr` of a different type.
    pub fn cast<U>(self) -> UserConstPtr<U> {
        UserConstPtr(self.0 as *const U)
    }

    /// Read into an uninitialized buffer with copy-from-user semantics.
    pub fn read_uninit(self) -> osvm::MemResult<MaybeUninit<T>> {
        let mut value = MaybeUninit::<T>::uninit();
        read_vm_bytes(self.0.cast::<u8>(), as_uninit_bytes(&mut value))?;
        Ok(value)
    }

    /// Read a typed value from user memory with copy-from-user semantics.
    pub fn read_vm(self) -> osvm::MemResult<T>
    where
        T: AnyBitPattern,
    {
        let value = self.read_uninit()?;
        Ok(unsafe { value.assume_init() })
    }

    /// Load a vector of values from user memory.
    pub fn load_vm_vec(self, len: usize) -> osvm::MemResult<Vec<T>>
    where
        T: AnyBitPattern,
    {
        let mut vec = Vec::with_capacity(len);
        read_vm_bytes(
            self.0.cast::<u8>(),
            spare_as_uninit_bytes(&mut vec.spare_capacity_mut()[..len]),
        )?;
        unsafe { vec.set_len(len) };
        Ok(vec)
    }
}

impl<T> VirtPtr for UserConstPtr<T> {
    type Target = T;

    fn as_ptr(self) -> *const Self::Target {
        self.0
    }
}

impl UserConstPtr<c_char> {
    /// Load a null-terminated string from user memory.
    pub fn load_string(self) -> KResult<String> {
        #[allow(clippy::unnecessary_cast)]
        let bytes = load_vec_until_null(self.0 as *const u8)?;
        String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
    }
}
