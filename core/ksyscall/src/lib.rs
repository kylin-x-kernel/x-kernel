// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Syscall adapter crate.
//!
//! `ksyscall` owns syscall number dispatch, Linux ABI decoding, user-pointer
//! marshalling, and routing to the real resource owners. It does not own the
//! long-lived state machines behind those syscalls.

#![no_std]
#![allow(missing_docs)]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod dispatch;
mod io_mpx;
mod ipc;
mod sync;
mod sys;
mod task;
mod time;
mod vfs;

pub use dispatch::dispatch_irq_syscall;
pub use ktty::terminal;
pub use posix_fs::file;
pub use sys::sys_getrandom;
#[cfg(feature = "tee")]
pub use tee_kernel::tee;
