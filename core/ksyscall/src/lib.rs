// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Syscall implementation crate.

#![no_std]
#![allow(missing_docs)]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod dispatch;
mod fs;
mod io_mpx;
mod sync;
mod sys;
mod task;

pub use dispatch::dispatch_irq_syscall;
pub use ktty::terminal;
pub use posix_fs::file;
pub use sys::sys_getrandom;
#[cfg(feature = "tee")]
pub use tee_kernel::tee;
