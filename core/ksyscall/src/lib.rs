// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Syscall implementation crate.

#![no_std]
#![feature(likely_unlikely)]
#![feature(bstr)]
#![allow(missing_docs)]

#[macro_use]
extern crate klogger;

extern crate alloc;

mod dispatch;
mod fs;
mod io_mpx;
mod ipc;
mod mm;
mod net;
mod resources;
mod signal;
mod sync;
mod sys;
mod task;
mod time;

pub use dispatch::dispatch_irq_syscall;
pub use kserveices::{file, io};
pub use sys::sys_getrandom;
pub mod kernel {
    pub use kserveices::vfs;
}
pub use kserveices::{socket, terminal};
#[cfg(feature = "tee")]
pub use tee_kernel::tee;
