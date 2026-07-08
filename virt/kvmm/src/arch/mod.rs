// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture-specific VMM trait and per-arch modules.

#[cfg(target_arch = "aarch64")]
pub mod aarch64;

#[cfg(target_arch = "x86_64")]
pub mod x86_64;

#[cfg(target_arch = "riscv64")]
pub mod riscv64;

use crate::{
    mm::GuestMem,
    vcpu::{ExitAction, Vcpu},
};

/// Architecture VMM hooks.
///
/// Each architecture implements these four functions to drive
/// the guest execution cycle in [`vmm_run_vcpu`](crate::vcpu::vmm_run_vcpu).
pub trait VmmArch {
    /// Per-architecture vCPU state (GPRs, sysregs, host save area).
    /// Must be `#[repr(C)]` with ABI-stable layout for assembly.
    type ArchVcpu: Default + Send;

    /// Second-stage page table type for this architecture.
    type GuestMem: GuestMem + Send + Sync;

    /// Initialize vCPU state with guest entry point and stack pointer.
    ///
    /// Each architecture translates `entry`/`sp` into the appropriate
    /// registers (ELR, vsepc, VMCS guest RIP, etc.). Returns `false`
    /// on failure (e.g. x86 VMCS allocation).
    fn init_vcpu(vcpu: &mut Vcpu<Self>, entry: u64, sp: u64) -> bool
    where
        Self: Sized;

    /// Restore guest context before entering the guest.
    fn restore_guest_ctx(vcpu: &mut Vcpu<Self>)
    where
        Self: Sized;

    /// Enter the guest (eret / vmlaunch / sret).
    /// Returns `true` on success, `false` on entry failure.
    fn enter_guest(vcpu: &mut Vcpu<Self>) -> bool
    where
        Self: Sized;

    /// Handle a VM exit and return the action to take.
    fn exit_handler(vcpu: &mut Vcpu<Self>) -> ExitAction
    where
        Self: Sized;

    /// Save guest context after exiting the guest.
    fn save_guest_ctx(vcpu: &mut Vcpu<Self>)
    where
        Self: Sized;

    /// Guest selftest code pointer and size in bytes.
    ///
    /// Returns `(start_ptr, size)` of the guest test binary in kernel
    /// `.text`. The loader copies this to a fresh page before execution.
    fn guest_test_code() -> (*const u8, usize);

    /// Per-CPU hardware init (idempotent). Called on each physical CPU
    /// before running vCPUs. Returns `false` on failure.
    fn percpu_hw_init() -> bool {
        true
    }

    /// Activate second-stage page table for a vCPU.
    ///
    /// Default implementation writes the hardware register (VTTBR_EL2
    /// on AArch64, hgatp on RISC-V). x86_64 overrides to write EPTP
    /// into the vCPU's VMCS.
    fn activate_guest_mem(vcpu: &mut Vcpu<Self>, guest_mem: &Self::GuestMem)
    where
        Self: Sized,
    {
        let _ = vcpu;
        guest_mem.activate();
    }

    /// Teardown hardware state before dropping a vCPU.
    ///
    /// Called at the end of `vmm_run_vcpu` before the `Vcpu` is dropped.
    /// x86_64 uses this to `vmclear` the VMCS before the page is freed.
    fn teardown_vcpu(_vcpu: &mut Vcpu<Self>)
    where
        Self: Sized,
    {
    }
}
