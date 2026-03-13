// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel-facing APIs for user space services and syscall glue.

#![no_std]
#![feature(likely_unlikely)]
#![feature(bstr)]
#![allow(missing_docs)]
#![allow(clippy::not_unsafe_ptr_arg_deref)]

#[macro_use]
extern crate klogger;

extern crate alloc;

pub mod file;
pub mod io;
pub mod mm;
pub mod signal;
pub mod socket;
pub mod syscall;
pub mod task;
#[cfg(feature = "tee")]
pub use tee_kernel::tee;
pub mod terminal;
pub mod time;

#[cfg(unittest)]
mod unittest_task;
#[cfg(unittest)]
pub use unittest_task::{register_unittest_runtime, run_with_test_user_thread};
pub mod vfs;

/// Initializes VFS, /proc/interrupts accounting, and alarm task.
pub fn init() {
    info!("Initialize VFS...");
    vfs::mount_all().expect("Failed to mount vfs");

    info!("Initialize /proc/interrupts...");
    ktask::register_timer_callback(|_| {
        time::inc_irq_cnt();
    });

    info!("Initialize alarm...");
    kcore::time::spawn_alarm_task();
}
