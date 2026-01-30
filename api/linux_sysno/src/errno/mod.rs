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

    /// Creates a new `Errno`.
    pub fn new(num: i32) -> Self {
        Self(num)
    }

    /// Converts the `Errno` into a raw `i32`.
    pub fn into_raw(self) -> i32 {
        self.0
    }

    /// Returns true if the error code is valid (i.e., less than 4096).
    pub fn is_valid(&self) -> bool {
        self.0 < 4096
    }

    /// Converts a raw syscall return value to a result.
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

pub trait ErrnoSentinel: Sized {
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

#[cfg(test)]
mod test {
    use super::*;

    #[test]
    fn basic() {
        assert_eq!(Errno::ENOENT.name(), Some("ENOENT"));
        assert_eq!(
            Errno::ENOENT.description(),
            Some("No such file or directory")
        );
        #[cfg(feature = "std")]
        {
            assert_eq!(
                format!("{}", Errno::ENOENT),
                "-2 ENOENT (No such file or directory)"
            );
            assert_eq!(format!("{:?}", Errno::ENOENT), "ENOENT");
        }
    }

    #[test]
    fn from_ret() {
        assert_eq!(Errno::from_ret(-2isize as usize), Err(Errno::ENOENT));
        assert_eq!(Errno::from_ret(2), Ok(2));
    }

    #[cfg(feature = "std")]
    #[test]
    fn io_error() {
        use std::io;

        assert_eq!(
            io::Error::from(Errno::ENOENT).kind(),
            io::ErrorKind::NotFound
        );

        assert_eq!(
            Errno::from_io_error(io::Error::from_raw_os_error(2)),
            Some(Errno::ENOENT)
        );

        assert_eq!(
            Errno::from_io_error(io::Error::new(io::ErrorKind::Other, "")),
            None
        );
    }

    #[cfg(feature = "std")]
    #[test]
    fn last_errno() {
        assert_eq!(
            Errno::result(unsafe {
                libc::open(
                    b"this_should_not_exist\0".as_ptr() as *const _,
                    libc::O_RDONLY,
                )
            }),
            Err(Errno::ENOENT)
        );
    }
}
