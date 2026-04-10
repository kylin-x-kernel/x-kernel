// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! hardware abstraction layer, provides unified APIs for
//! platform-specific operations.
//!
//! It does the bootstrapping and initialization process for the specified
//! platform, and provides useful operations on the hardware.
//!
//! Currently supported platforms (specify by cargo features):
//!
//! - `x86-pc`: Standard PC with x86_64 ISA.
//! - `riscv64-qemu-virt`: QEMU virt machine with RISC-V ISA.
//! - `aarch64-qemu-virt`: QEMU virt machine with AArch64 ISA.
//! - `aarch64-raspi`: Raspberry Pi with AArch64 ISA.
//! - `dummy`: If none of the above platform is selected, the dummy platform
//!   will be used. In this platform, most of the operations are no-op or
//!   `unimplemented!()`. This platform is mainly used for [cargo test].
//!
//! # Cargo Features
//!
//! - `smp`: Enable SMP (symmetric multiprocessing) support.
//! - `fp-simd`: Enable floating-point and SIMD support.
//! - `paging`: Enable page table manipulation.
//! - `tls`: Enable kernel space thread-local storage support.
//! - `rtc`: Enable real-time clock support.
//! - User space support is always enabled.

#![no_std]
#![feature(doc_cfg)]
#![allow(rustdoc::broken_intra_doc_links)]

extern crate alloc;

#[allow(unused_imports)]
#[macro_use]
extern crate log;

#[allow(unused_imports)]
#[macro_use]
extern crate memaddr;

// mod dummy;

pub mod console;
pub mod firmware;
pub mod mem;
pub mod percpu;
pub mod rtc;
pub mod time;

#[cfg(feature = "tls")]
pub mod tls;

pub mod irq;
pub mod paging;
pub use firmware::cmdline;

/// CPU power management.
pub mod power {
    #[cfg(feature = "smp")]
    pub use kplat::sys::boot_ap;
    pub use kplat::sys::shutdown;
}

/// Trap handling.
pub mod trap {
    pub use kcpu::excp::{IRQ, PAGE_FAULT, PageFaultFlags, register_trap_handler};
}

/// CPU register states for context switching.
///
/// There are two types of context:
///
/// - [`TaskContext`][kcpu::TaskContext]: The context of a task.
/// - [`TrapFrame`][kcpu::TrapFrame]: The context of an interrupt or an exception.
///
/// In addition, this module exposes helpers to *observe* the currently active
/// trap context on the current CPU:
///
/// - [`active_exception_context`]: Returns a best-effort reference to the trapframe
///   that is currently active on this CPU, if any.
///   The returned reference is **short-lived** and only valid while the CPU
///   remains in the corresponding trap context. It must not be stored.
///
/// - [`with_active_exception_context`]: Executes a closure with the currently active
///   trapframe (or `None` if not in a trap). This is intended for diagnostic
///   paths such as watchdogs or backtrace collection.
pub mod context {
    pub use kcpu::{
        TaskContext, TrapFrame, active_exception_context, with_active_exception_context,
    };
}

pub use kcpu::{instrs as asm, userspace as uspace};
#[cfg(feature = "smp")]
pub use kplat::boot::final_init_ap as final_init_secondary;

#[cfg(feature = "nmi")]
pub mod nmi {
    pub use kplat::nm_irq::{enable, init, register_nmi_handler};
}

#[cfg(feature = "pmu")]
pub mod pmu {
    pub use kplat::perf::{
        PerfCb, on_overflow as dispatch_irq_overflows, reg_cb as register_overflow_handler,
    };
}
#[inline]
pub fn boot_info(arg: usize) -> &'static boot_info::BootInfo {
    let boot_info = unsafe { &*(arg as *const boot_info::BootInfo) };
    assert!(boot_info.is_valid(), "invalid boot info");
    boot_info
}

pub fn early_driver_init() {
    kplat::boot::early_driver_init();
}

/// Completes platform initialization from the unified boot handoff payload.
pub fn final_init(boot_info: &boot_info::BootInfo) {
    kplat::boot::final_init(boot_info);
}

macro_rules! addr_of_sym {
    ($e:ident) => {
        $e as *const () as usize
    };
}
pub(crate) use addr_of_sym;
