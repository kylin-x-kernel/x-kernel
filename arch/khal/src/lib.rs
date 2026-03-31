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

use lazyinit::LazyInit;

#[allow(unused_imports)]
#[macro_use]
extern crate log;

#[allow(unused_imports)]
#[macro_use]
extern crate memaddr;

// mod dummy;

pub mod mem;
pub mod percpu;
pub mod rsvd_mem;
pub mod time;

#[cfg(feature = "tls")]
pub mod tls;

pub mod irq;
pub mod paging;

/// Console input and output.
pub mod console {
    pub use kplat::io::{interrupt_id, read_data, write_data};
}

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
pub use kplat::boot::{
    early_init_ap as early_init_secondary, final_init_ap as final_init_secondary,
};

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

const CMDLINE_BUF_SIZE: usize = 2048;
static CMDLINE: LazyInit<([u8; CMDLINE_BUF_SIZE], usize)> = LazyInit::new();

/// Returns the kernel command line from the unified boot handoff.
///
/// BootInfo-provided command line takes precedence. When it is absent,
/// fall back to `/chosen/bootargs` from the device tree.
pub fn cmdline() -> Option<&'static str> {
    if let Some((buf, len)) = CMDLINE.get() {
        if *len > 0 {
            return core::str::from_utf8(&buf[..*len]).ok();
        }
        return None;
    }
    of::chosen_bootargs()
}

/// Initializes the platform from the unified boot handoff payload.
/// This function should be called as early as possible.
pub fn early_init(boot_info: &boot_info::BootInfo) {
    let dtb_vaddr = if boot_info.dtb_addr != 0 {
        Some(mem::p2v(boot_info.dtb_addr.into()).as_usize() as *const u8)
    } else {
        None
    };
    if let Some(ptr) = dtb_vaddr {
        let _ = unsafe { of::init_device_tree_ptr(ptr) };
        info!("device tree initialized");
        if let Some(model) = of::root_model() {
            info!("of root model: {model}");
        }
        if let Some(compatible) = of::root_compatible() {
            info!("of root compatible: {compatible}");
        }
        if let Some(bootargs) = of::chosen_bootargs() {
            info!("of chosen bootargs: {bootargs}");
        }
    } else if boot_info.rsdp_addr != 0 {
        let _ = acpi::init(boot_info.rsdp_addr);
    }
    let mut cmdline_buf = [0; CMDLINE_BUF_SIZE];
    let cmdline_len = if let Some(cmdline) = boot_info.cmdline() {
        let bytes = cmdline.as_bytes();
        let len = bytes.len().min(CMDLINE_BUF_SIZE);
        cmdline_buf[..len].copy_from_slice(&bytes[..len]);
        len
    } else if let Some(cmdline) = of::chosen_bootargs() {
        let bytes = cmdline.as_bytes();
        let len = bytes.len().min(CMDLINE_BUF_SIZE);
        cmdline_buf[..len].copy_from_slice(&bytes[..len]);
        len
    } else {
        0
    };
    CMDLINE.init_once((cmdline_buf, cmdline_len));
    kplat::boot::early_init(boot_info);
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
