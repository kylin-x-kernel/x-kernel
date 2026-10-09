// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Linux syscall numbers, raw argument carriers, errno values, and indexed collections.
//!
//! [`Sysno`] selects the compilation target's table; architecture modules expose
//! additional tables when their Cargo features are enabled. Table membership
//! does not imply that X-Kernel implements the syscall. Use [`Sysno::new`] to
//! validate untrusted numbers: the integer `From` implementations panic on gaps.
//!
//! [`SysnoSet`] and [`SysnoMap`] store target-specific keys without heap allocation.
//! [`SyscallArgs`] preserves raw register values without dereferencing pointers.
//! [`Errno`] decodes Linux return registers and is distinct from kernel error kinds.
//! The legacy [`syscall!`] and [`raw_syscall!`] macros have no invocation backend
//! in this kernel fork; use the architecture/runtime entry path instead.
//!
//! # Examples
//!
//! ```
//! use linux_sysno::{Errno, Sysno, SysnoSet};
//! let allowed = SysnoSet::new(&[Sysno::read, Sysno::write]);
//! assert!(allowed.contains(Sysno::new(Sysno::read.id() as usize).unwrap()));
//! assert_eq!(Errno::from_ret(-2isize as usize), Err(Errno::ENOENT));
//! ```

#![no_std]
#![deny(clippy::all)]

#[macro_use]
mod macros;

mod arch;
mod args;
mod errno;
mod map;
mod set;

pub use arch::*;
pub use args::SyscallArgs;
pub use errno::{Errno, ErrnoSentinel};
pub use map::*;
pub use set::*;

#[unittest::mod_test]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;

    #[def_test]
    fn test_name() {
        assert_eq!(Sysno::write.name(), "write");
        assert_eq!(Sysno::fsopen.name(), "fsopen");
    }

    #[cfg(target_arch = "x86_64")]
    #[def_test]
    fn test_syscallno() {
        assert_eq!(Sysno::from(2), Sysno::open);
        assert_eq!(Sysno::new(2), Some(Sysno::open));
        assert_eq!(Sysno::new(-1i32 as usize), None);
        assert_eq!(Sysno::new(1024), None);
    }

    #[def_test]
    fn test_first() {
        #[cfg(target_arch = "x86_64")]
        assert_eq!(Sysno::first(), Sysno::read);

        #[cfg(target_arch = "x86")]
        assert_eq!(Sysno::first(), Sysno::restart_syscall);
    }

    #[def_test]
    fn test_syscall_len() {
        assert!(Sysno::table_size() > 300);
        assert!(Sysno::table_size() < 1000);
    }
}
