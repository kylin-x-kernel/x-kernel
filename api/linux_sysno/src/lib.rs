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

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
