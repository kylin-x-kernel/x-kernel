// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Syscall adapter crate.
//!
//! `ksyscall` owns syscall number dispatch, Linux ABI decoding, user-pointer
//! marshalling, and routing to the real resource owners. It does not own the
//! long-lived state machines behind those syscalls.

#![no_std]
#![allow(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): 69 legacy diagnostic sites need \
              individual failure-contract review; new code remains subject to the \
              no-legitimate-failure-panic rule"
)]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod arch;
mod cpuset;
mod dispatch;
mod io_mpx;
mod ipc;
mod sync;
mod sys;
mod task;
mod time;
mod vfs;

pub use dispatch::handle_syscall;
pub use ktty::terminal;
pub use posix_fs::file;
pub use sys::sys_getrandom;
#[cfg(feature = "tee")]
pub use tee_kernel::tee;
