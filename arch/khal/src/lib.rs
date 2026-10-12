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
//! - `kplat-x86_64`: QEMU q35 machine with x86_64 ISA.
//! - `kplat-riscv64`: QEMU virt machine with RISC-V ISA.
//! - `kplat-aarch64`: QEMU virt and RK3588 machines with AArch64 ISA.
//! - `kplat-loongarch64`: QEMU virt machine with LoongArch64 ISA.
//! - `dummy`: If none of the above platform is selected, the dummy platform
//!   will be used. In this platform, most of the operations are no-op or
//!   `unimplemented!()`. This platform is mainly used for [cargo test].
//!
//! # Cargo Features
//!
//! - `smp`: Enable SMP (symmetric multiprocessing) support.
//! - `fp-simd`: Enable floating-point and SIMD support.
//! - `paging`: Enable page table manipulation.
//! - User space support is always enabled.

#![allow(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): legacy khal has 18 diagnostic \
              sites, too many to judge individually in this migration; new code remains reviewed"
)]
#![no_std]

extern crate alloc;

#[macro_use]
extern crate log;

extern crate memaddr;

// mod dummy;

pub mod console;
pub mod firmware;
pub mod mem;
pub mod percpu;
pub mod time;

pub mod irq;
pub mod paging;
pub mod power;
pub mod trap;
pub use firmware::cmdline;

/// CPU register states for context switching.
///
/// There are two types of context:
///
/// - [`TaskContext`][kcpu::TaskContext]: The context of a task.
/// - [`TrapFrame`][kcpu::TrapFrame]: The context of an interrupt or an exception.
///
/// Current-task identity is also provided by `kcpu`: [`current_task_ptr`](context::current_task_ptr)
/// observes an opaque pointer and [`set_current_task_ptr`](context::set_current_task_ptr)
/// publishes it under scheduler-owned lifetime and migration exclusion. This
/// module only re-exports those architecture operations; it owns no task storage.
///
/// In addition, this module exposes helpers to *observe* the currently active
/// trap context on the current CPU:
///
/// - [`active_exception_context`](context::active_exception_context): Returns a best-effort reference to the trapframe
///   that is currently active on this CPU, if any.
///   The returned reference is **short-lived** and only valid while the CPU
///   remains in the corresponding trap context. It must not be stored.
///
/// - [`with_active_exception_context`](context::with_active_exception_context): Executes a closure with the currently active
///   trapframe (or `None` if not in a trap). This is intended for diagnostic
///   paths such as watchdogs or backtrace collection.
///
/// - [`in_exception_context`](context::in_exception_context): Returns whether the current CPU is still inside
///   an exception context without exposing the trapframe itself.
///
/// - [`suspend_active_exception_context`](context::suspend_active_exception_context) and [`resume_active_exception_context`](context::resume_active_exception_context):
///   Temporarily remove and later restore the current CPU marker across a task
///   switch, so a sleepable trap backend does not leave a stale marker on the
///   old CPU and still resumes with the marker on the new CPU.
pub mod context {
    pub use kcpu::{
        SuspendedExceptionContext, TaskContext, TrapFrame, active_exception_context,
        current_task_ptr, in_exception_context, resume_active_exception_context,
        set_current_task_ptr, suspend_active_exception_context, with_active_exception_context,
    };
}

pub use kcpu::{instrs as asm, userspace as uspace};
#[cfg(feature = "smp")]
pub use kplat::boot::final_init_ap as final_init_secondary;

#[cfg(feature = "nmi")]
pub mod nmi {
    pub use kplat::nm_irq::{
        NmiCb, NmiMode, NmiSourceInfo, early_init, enable_periodic_nmi, info, late_init, mode,
    };
}

#[cfg(feature = "pmu")]
pub mod pmu {
    pub use kplat::perf::{
        PerfCb, enable_irq, on_overflow as handle_overflows, reg_cb as register_overflow_handler,
        register_overflow_irq as register_overflow_dispatch,
    };
}

/// Quiesces the local NMI (or pseudo-NMI) source on the current CPU.
///
/// This is a no-op when the platform NMI facility is disabled. It is called
/// before parking a CPU on the final stop path so that an NMI-driven
/// hard-lockup watchdog can neither keep waking the parked CPU nor mistake
/// the intentional stop for a lockup and panic into a platform power-off.
#[inline]
pub fn quiesce_nmi() {
    #[cfg(feature = "nmi_pmu")]
    {
        // The periodic-source provider is per-CPU. Keep this CPU pinned while
        // it finds and stops the local source; its counter access is safe
        // against an in-flight NMI dispatch.
        let _guard = kspin::NoPreempt::new();
        kplat::nm_irq::quiesce_periodic_nmi();
    }
}
#[inline]
pub fn boot_info(arg: usize) -> &'static boot_info::BootInfo {
    // SAFETY: boot entry passes `arg` as the address of a live `BootInfo`
    // structure that remains valid for the kernel lifetime.
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
