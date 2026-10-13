// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[macro_use]
mod macros;

mod generated;

use core::fmt;

pub use self::generated::Errno;

impl Errno {
    /// Same as [`Errno::EDEADLK`].
    pub const EDEADLOCK: Self = Self::EDEADLK;
    /// Operation would block. This is the same as [`Errno::EAGAIN`].
    pub const EWOULDBLOCK: Self = Self::EAGAIN;

    /// Wraps `num` without validation, conventionally a positive Linux errno.
    ///
    /// Zero, negative values, and unknown codes are representable. Validate
    /// externally supplied values before converting them to kernel errors.
    pub fn new(num: i32) -> Self {
        Self(num)
    }

    /// Converts the `Errno` into a raw `i32`.
    pub fn into_raw(self) -> i32 {
        self.0
    }

    /// Tests only whether the stored value is less than 4096.
    ///
    /// This also accepts zero and negative values; it does not establish a
    /// positive Linux errno or membership in the named errno table.
    pub fn is_valid(&self) -> bool {
        self.0 < 4096
    }

    /// Decodes a raw return register, preserving successful values unchanged.
    ///
    /// # Errors
    ///
    /// Register encodings of signed values from -4095 through -1 yield the
    /// corresponding positive errno. The encoding of -4096 is treated as success.
    #[inline(always)]
    pub fn from_ret(value: usize) -> Result<usize, Errno> {
        if value > -4096isize as usize {
            // Truncation of the error value is guaranteed to never occur due to
            // the above check. This is the same check that musl uses:
            // https://git.musl-libc.org/cgit/musl/tree/src/internal/syscall_ret.c?h=v1.1.15
            Err(Self(-(value as i32)))
        } else {
            Ok(value)
        }
    }

    /// Returns the name of the error. If the internal error code is unknown or
    /// invalid, `None` is returned.
    pub fn name(&self) -> Option<&'static str> {
        self.name_and_description().map(|x| x.0)
    }

    /// Returns the error description. If the internal error code is unknown or
    /// invalid, `None` is returned.
    pub fn description(&self) -> Option<&'static str> {
        self.name_and_description().map(|x| x.1)
    }
}

impl fmt::Display for Errno {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.name_and_description() {
            Some((name, description)) => {
                write!(f, "{} {name} ({description})", -self.0)
            }
            None => {
                if self.is_valid() {
                    write!(f, "{}", -self.0)
                } else {
                    write!(f, "Invalid errno {:#x}", self.0)
                }
            }
        }
    }
}

impl fmt::Debug for Errno {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self.name() {
            Some(name) => f.write_str(name),
            None => write!(f, "Errno({})", self.0),
        }
    }
}

/// Supplies the all-ones failure sentinel used by libc-style return types.
///
/// This does not store an errno or decode a raw Linux return register.
pub trait ErrnoSentinel: Sized {
    /// Returns -1 for signed integers, `usize::MAX`, or the all-ones pointer.
    fn sentinel() -> Self;
}

impl ErrnoSentinel for isize {
    fn sentinel() -> Self {
        -1
    }
}

impl ErrnoSentinel for i32 {
    fn sentinel() -> Self {
        -1
    }
}

impl ErrnoSentinel for i64 {
    fn sentinel() -> Self {
        -1
    }
}

impl ErrnoSentinel for *mut core::ffi::c_void {
    fn sentinel() -> Self {
        -1isize as *mut core::ffi::c_void
    }
}

impl ErrnoSentinel for usize {
    fn sentinel() -> Self {
        usize::MAX
    }
}

unittest::path_mod_test!(test, "../../ut-tests/errno.rs");
