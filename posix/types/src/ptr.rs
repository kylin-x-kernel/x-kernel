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
    fmt::Debug,
    mem::{MaybeUninit, size_of, size_of_val},
    ptr, slice,
};

use kerrno::{KError, KResult};
use osvm::{VirtMutPtr, VirtPtr, load_vec_until_null, read_vm_bytes, write_vm_bytes};

fn as_uninit_bytes<T>(value: &mut MaybeUninit<T>) -> &mut [MaybeUninit<u8>] {
    // SAFETY: `MaybeUninit<T>` may always be viewed as an equally sized byte slice.
    unsafe {
        slice::from_raw_parts_mut(value.as_mut_ptr().cast::<MaybeUninit<u8>>(), size_of::<T>())
    }
}

fn spare_as_uninit_bytes<T>(spare: &mut [MaybeUninit<T>]) -> &mut [MaybeUninit<u8>] {
    // SAFETY: a `[MaybeUninit<T>]` spare-capacity slice may always be viewed as
    // an equally sized byte slice for copy-from-user initialization.
    unsafe {
        slice::from_raw_parts_mut(
            spare.as_mut_ptr().cast::<MaybeUninit<u8>>(),
            size_of_val(spare),
        )
    }
}

/// A by-value syscall ABI object that can be copied from user memory.
///
/// # Safety
///
/// Implementers must ensure that any byte pattern copied from user memory is a
/// valid initialized value of `Self`.
pub unsafe trait UserRead {}

/// A by-value syscall ABI object that can be copied to user memory.
///
/// # Safety
///
/// Implementers must ensure that values of `Self` always expose a fully
/// initialized byte representation when viewed as raw bytes for copy-to-user.
/// In practice this means the type must not contain implicit padding bytes, or
/// it must model those bytes as explicit fields that are always initialized.
pub unsafe trait UserWrite {}

macro_rules! impl_user_read_for_scalars {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: primitive scalars are valid syscall by-value carriers.
            unsafe impl UserRead for $ty {}
        )*
    };
}

impl_user_read_for_scalars!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);
// SAFETY: `()` has no bytes and is trivially safe to copy from user memory.
unsafe impl UserRead for () {}
macro_rules! impl_user_write_for_scalars {
    ($($ty:ty),* $(,)?) => {
        $(
            // SAFETY: primitive scalars always expose fully initialized bytes.
            unsafe impl UserWrite for $ty {}
        )*
    };
}

impl_user_write_for_scalars!(i8, i16, i32, i64, isize, u8, u16, u32, u64, usize);

// SAFETY: fixed-size arrays are valid when each element upholds the same contract.
unsafe impl<T: UserRead, const N: usize> UserRead for [T; N] where [T; N]: Copy {}
// SAFETY: fixed-size arrays are valid when each element upholds the same contract.
unsafe impl<T: UserWrite, const N: usize> UserWrite for [T; N] {}
// SAFETY: raw pointers are copied by value without dereferencing.
unsafe impl<T> UserWrite for *const T {}
// SAFETY: raw pointers are copied by value without dereferencing.
unsafe impl<T> UserWrite for *mut T {}

/// A mutable user virtual address, copied by value without validating its target.
///
/// Copy helpers require the `osvm` current-address-space provider. They may
/// fault or block; use them in initialized thread context, not early boot or a
/// hardware IRQ. `cast`, `is_null`, and `check_non_null` perform no memory access.
/// Non-null does not imply mapped, writable, aligned, or exclusively owned.
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

    /// Converts this mutable user pointer to a read-only one.
    pub fn as_const(self) -> UserConstPtr<T> {
        UserConstPtr(self.0.cast_const())
    }

    /// Copies `size_of::<T>()` bytes into uninitialized kernel storage.
    ///
    /// The returned `MaybeUninit<T>` is not proof that those bytes form a valid
    /// `T`; use `read_vm` when the `UserRead` representation contract applies.
    ///
    /// # Errors
    ///
    /// Propagates the `osvm` memory-access error for the user source range.
    pub fn read_uninit(self) -> osvm::MemResult<MaybeUninit<T>> {
        self.as_const().read_uninit()
    }

    /// Copies one ABI value; `T: UserRead` makes every copied bit pattern valid.
    ///
    /// This establishes representation validity, not permission or ABI field
    /// semantics, and does not provide an atomic snapshot of shared user memory.
    ///
    /// # Errors
    ///
    /// Propagates the `osvm` memory-access error for the user source range.
    pub fn read_vm(self) -> osvm::MemResult<T>
    where
        T: UserRead,
    {
        self.as_const().read_vm()
    }

    /// Allocates and copies `len` values from the current user address space.
    ///
    /// `len` is an element count. Callers must enforce a resource limit before
    /// allocation; this helper imposes no syscall-specific maximum.
    ///
    /// # Errors
    ///
    /// Propagates the `osvm` copy error and drops the temporary allocation.
    ///
    /// # Panics
    ///
    /// Capacity overflow can panic; allocation failure follows allocator policy.
    pub fn load_vm_vec(self, len: usize) -> osvm::MemResult<Vec<T>>
    where
        T: UserRead,
    {
        self.as_const().load_vm_vec(len)
    }

    /// Copies a value's initialized representation to the current user address space.
    ///
    /// # Errors
    ///
    /// Propagates the `osvm` write error; failed writes may have partial effects.
    pub fn write_vm(self, value: T) -> osvm::MemResult
    where
        T: UserWrite,
    {
        self.write_vm_slice(slice::from_ref(&value))
    }

    /// Copies all bytes of `data` into consecutive user memory.
    ///
    /// `T: UserWrite` guarantees initialized output bytes, including explicit
    /// padding. This does not validate the semantic values in those bytes.
    ///
    /// # Errors
    ///
    /// Propagates the `osvm` write error; the copy is not transactional.
    pub fn write_vm_slice(self, data: &[T]) -> osvm::MemResult
    where
        T: UserWrite,
    {
        // SAFETY: a live slice may be reinterpreted as a same-sized byte slice
        // for copy-to-user without changing layout.
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

impl<T> Debug for UserPtr<T> {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_tuple("UserPtr").field(&self.0).finish()
    }
}

// SAFETY: `UserPtr` is just a bare pointer, and its memory layout is equivalent to *mut T,
// Any bit pattern is legal for naked pointers, so it meets AnyBitPattern.
unsafe impl<T: 'static> bytemuck::Zeroable for UserPtr<T> {}
// SAFETY: `UserPtr<T>` is a transparent raw-pointer wrapper, so every pointer
// bit pattern is valid for the type.
unsafe impl<T: 'static> bytemuck::AnyBitPattern for UserPtr<T> {}

// SAFETY: Reading/writing a `UserPtr` copies the raw pointer value (usize-sized)
// without dereferencing it. Any bit pattern is valid for a pointer wrapper.
unsafe impl<T> UserRead for UserPtr<T> {}
// SAFETY: Reading/writing a `UserPtr` copies the raw pointer value (usize-sized)
// without dereferencing it. Any bit pattern is valid for a pointer wrapper.
unsafe impl<T> UserWrite for UserPtr<T> {}

/// A read-only user virtual address, copied by value without validating its target.
///
/// Copy helpers require initialized `osvm` current-address-space services and
/// may fault or block. Other threads can modify user memory during or after a
/// copy, so copied fields still require semantic validation. Pure pointer
/// operations need no process context.
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

impl<T> From<UserPtr<T>> for UserConstPtr<T> {
    fn from(value: UserPtr<T>) -> Self {
        value.as_const()
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

    /// Copies `size_of::<T>()` bytes into uninitialized kernel storage.
    ///
    /// The returned `MaybeUninit<T>` is not proof that those bytes form a valid
    /// `T`; use `read_vm` when the `UserRead` representation contract applies.
    ///
    /// # Errors
    ///
    /// Propagates the `osvm` memory-access error for the user source range.
    pub fn read_uninit(self) -> osvm::MemResult<MaybeUninit<T>> {
        let mut value = MaybeUninit::<T>::uninit();
        read_vm_bytes(self.0.cast::<u8>(), as_uninit_bytes(&mut value))?;
        Ok(value)
    }

    /// Copies one ABI value; `T: UserRead` makes every copied bit pattern valid.
    ///
    /// This establishes representation validity, not permission or ABI field
    /// semantics, and does not provide an atomic snapshot of shared user memory.
    ///
    /// # Errors
    ///
    /// Propagates the `osvm` memory-access error for the user source range.
    pub fn read_vm(self) -> osvm::MemResult<T>
    where
        T: UserRead,
    {
        let value = self.read_uninit()?;
        // SAFETY: `T: UserRead` promises copied bytes form a valid initialized `T`.
        Ok(unsafe { value.assume_init() })
    }

    /// Allocates and copies `len` values from the current user address space.
    ///
    /// `len` is an element count. Callers must enforce a resource limit before
    /// allocation; this helper imposes no syscall-specific maximum.
    ///
    /// # Errors
    ///
    /// Propagates the `osvm` copy error and drops the temporary allocation.
    ///
    /// # Panics
    ///
    /// Capacity overflow can panic; allocation failure follows allocator policy.
    pub fn load_vm_vec(self, len: usize) -> osvm::MemResult<Vec<T>>
    where
        T: UserRead,
    {
        let mut vec = Vec::with_capacity(len);
        read_vm_bytes(
            self.0.cast::<u8>(),
            spare_as_uninit_bytes(&mut vec.spare_capacity_mut()[..len]),
        )?;
        // SAFETY: `read_vm_bytes` initialized exactly `len` elements worth of bytes.
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
    /// Loads a NUL-terminated UTF-8 string from the current user address space.
    ///
    /// No caller-selected bound is enforced here; prefer the bounded helper for
    /// hostile strings. The terminator is not included in the returned string.
    ///
    /// # Errors
    ///
    /// Propagates the `osvm` string-copy error or returns `IllegalBytes` for
    /// invalid UTF-8. Allocation failure follows allocator policy.
    pub fn load_string(self) -> KResult<String> {
        #[expect(
            clippy::allow_attributes,
            reason = "CG-024-lint-exceptions: C char is signed on some architectures; user-memory \
                      helpers require byte pointers"
        )]
        #[allow(
            clippy::unnecessary_cast,
            reason = "CG-024-lint-exceptions: clippy::unnecessary_cast: C char is signed on some \
                      architectures; user-memory helpers require byte pointers"
        )]
        let bytes = load_vec_until_null(self.0 as *const u8)?;
        String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
    }

    /// Load a null-terminated string whose byte length must not exceed
    /// `max_len`, excluding the terminator.
    ///
    /// # Errors
    ///
    /// Returns `InvalidInput` if no NUL occurs within the bound, `IllegalBytes`
    /// for invalid UTF-8, or the user-memory copy error. Requires initialized
    /// current-address-space and allocation services.
    pub fn load_string_with_max_len(self, max_len: usize) -> KResult<String> {
        let bytes = self.load_bytes_with_max_len(max_len).map_err(|err| {
            if err.canonicalize() == KError::OutOfRange {
                KError::InvalidInput
            } else {
                err
            }
        })?;
        String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
    }

    /// Loads a NUL-terminated byte string with a bounded payload length.
    ///
    /// Unlike [`Self::load_string_with_max_len`], this preserves non-UTF-8
    /// bytes for Linux ABI names whose value is an opaque byte sequence.
    /// At most `max_len + 1` bytes are probed, with the last reserved for NUL;
    /// the result excludes the terminator. The caller must bound allocation.
    ///
    /// # Errors
    ///
    /// Returns `OutOfRange` when a non-NUL byte is found at the payload limit,
    /// or propagates the user-memory copy error. Earlier reads are not a stable
    /// snapshot of concurrently writable user memory.
    pub fn load_bytes_with_max_len(self, max_len: usize) -> KResult<Vec<u8>> {
        let mut bytes = Vec::new();

        for offset in 0..=max_len {
            let mut byte = MaybeUninit::<u8>::uninit();
            #[expect(
                clippy::allow_attributes,
                reason = "CG-024-lint-exceptions: C char is signed on some architectures; \
                          user-memory helpers require byte pointers"
            )]
            #[allow(
                clippy::unnecessary_cast,
                reason = "CG-024-lint-exceptions: clippy::unnecessary_cast: C char is signed on \
                          some architectures; user-memory helpers require byte pointers"
            )]
            let user_ptr = (self.0 as *const u8).wrapping_add(offset);
            read_vm_bytes(user_ptr, as_uninit_bytes(&mut byte))?;
            // SAFETY: `read_vm_bytes` initialized exactly one byte in `byte`.
            let byte = unsafe { byte.assume_init() };

            if byte == 0 {
                return Ok(bytes);
            }
            if bytes.len() == max_len {
                return Err(KError::OutOfRange);
            }
            bytes.push(byte);
        }

        Err(KError::OutOfRange)
    }
}
