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
pub mod tee;
pub mod terminal;
pub mod time;
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
