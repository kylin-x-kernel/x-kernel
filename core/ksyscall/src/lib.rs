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
mod net;
mod sync;
mod sys;
mod task;

pub use dispatch::dispatch_irq_syscall;
pub use kservices::file;
pub use sys::sys_getrandom;
pub mod kernel {
    pub use kservices::vfs;
}
pub use kservices::{socket, terminal};
#[cfg(feature = "tee")]
pub use tee_kernel::tee;
