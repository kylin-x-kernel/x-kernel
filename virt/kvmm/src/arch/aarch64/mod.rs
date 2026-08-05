// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 VHE (Virtualization Host Extensions) VMM implementation.
//!
//! The kernel runs at EL2 with `HCR_EL2.E2H=1`. Guest vCPUs execute
//! at EL1 via `eret`, trapping back to EL2 on WFI/HVC/Data Abort.

use super::VmmArch;
use crate::vcpu::{ExitAction, Vcpu};

core::arch::global_asm!(include_str!("el2_vmcs.S"));
core::arch::global_asm!(include_str!("vcpu_ctx.S"));
core::arch::global_asm!(include_str!("guest_vec.S"));
core::arch::global_asm!(include_str!("guest_test.S"));

unsafe extern "C" {
    fn el2_enter_guest(vcpu: *mut Aarch64Vcpu);
    fn kvmm_save_sysregs_el12(buf: *mut u8);
    fn kvmm_restore_sysregs_el12(buf: *mut u8);
    pub(crate) fn kvmm_guest_test_entry();
    pub(crate) fn kvmm_guest_test_entry_end();
}

/// ESR_EL2 exception class values.
const EC_WFI_WFE: u32 = 0x01;
const EC_HVC64: u32 = 0x16;
const EC_DATA_ABORT_LOWER: u32 = 0x24;

/// HVC hypercall numbers.
const HVC_DONE: u64 = 0;
const HVC_PRINT: u64 = 1;

/// AArch64 VHE VMM backend.
pub struct Aarch64Vhe;

/// AArch64 vCPU state matching avatar-next `vcpu_t` layout.
///
/// The first fields are accessed by assembly at fixed offsets.
/// Do not reorder without updating assembly constants.
#[repr(C)]
#[derive(Clone)]
pub struct Aarch64Vcpu {
    /// x0-x30 guest GPRs (31 regs, offset 0).
    pub gprs: [u64; 31],
    /// Guest SP_EL1 (offset 248).
    pub sp_el1: u64,
    /// Guest PC = ELR_EL2 (offset 256).
    pub elr: u64,
    /// Guest PSTATE = SPSR_EL2 (offset 264).
    pub spsr: u64,
    /// Host callee-saved registers + SP (13 regs, offset 272).
    pub host_ctx: [u64; 13],
    /// Saved host VBAR_EL2 (offset 376).
    pub host_vbar: u64,
    /// Saved host TPIDR (offset 384).
    pub host_tpidr: u64,
    /// Guest EL1 system registers (offset 392).
    pub sysregs: [u8; 128],
    /// Exit type set by vector entry (offset 520).
    /// 0=sync, 1=IRQ, 2=FIQ, 3=SError.
    pub exit_type: u64,
}

impl Default for Aarch64Vcpu {
    fn default() -> Self {
        Self {
            gprs: [0; 31],
            sp_el1: 0,
            elr: 0,
            spsr: 0,
            host_ctx: [0; 13],
            host_vbar: 0,
            host_tpidr: 0,
            sysregs: [0; 128],
            exit_type: 0,
        }
    }
}

#[inline]
fn read_esr_el2() -> u64 {
    let val: u64;
    // SAFETY: reading ESR_EL2 is always safe from EL2.
    unsafe { core::arch::asm!("mrs {}, esr_el2", out(reg) val) };
    val
}

#[inline]
fn read_far_el2() -> u64 {
    let val: u64;
    // SAFETY: reading FAR_EL2 is always safe from EL2.
    unsafe { core::arch::asm!("mrs {}, far_el2", out(reg) val) };
    val
}

fn advance_pc(vcpu: &mut Aarch64Vcpu, esr: u64) {
    let step = if (esr >> 25) & 1 == 1 { 4 } else { 2 };
    vcpu.elr += step;
}

fn handle_wfi(vcpu: &mut Aarch64Vcpu, esr: u64) -> ExitAction {
    advance_pc(vcpu, esr);
    ktask::sleep(ktime_types::TimeSpan::from_millis(1));
    ExitAction::Resume
}

fn handle_hvc(vcpu: &mut Aarch64Vcpu) -> ExitAction {
    let no = vcpu.gprs[0];
    match no {
        HVC_PRINT => {
            let _iter = vcpu.gprs[1];
            // if _iter.is_multiple_of(20) {
            //     log::info!("[VMM] HVC_PRINT: iter={} (aarch64)", _iter);
            // }
            ExitAction::Resume
        }
        HVC_DONE => {
            let tests = vcpu.gprs[1];
            log::info!("[VMM] HVC_DONE: {} tests complete (aarch64)", tests);
            ExitAction::VmExit
        }
        _ => {
            log::warn!("[VMM] Unknown HVC no={} (aarch64)", no);
            ExitAction::Resume
        }
    }
}

#[inline]
fn read_hpfar_el2() -> u64 {
    let val: u64;
    // SAFETY: reading HPFAR_EL2 is always safe from EL2.
    unsafe { core::arch::asm!("mrs {}, hpfar_el2", out(reg) val) };
    val
}

fn handle_data_abort(vcpu: &mut Vcpu<Aarch64Vhe>, esr: u64) -> ExitAction {
    let far = read_far_el2();
    let hpfar = read_hpfar_el2();
    let ipa = (hpfar << 8) | (far & 0xFFF);
    let is_write = (esr >> 6) & 1 != 0;

    // Try MMIO dispatch via the VM's device bus.
    let mut bus = vcpu.vm.mmio_bus().lock();
    if let Some(_val) = bus.handle(ipa, is_write, 4, 0) {
        drop(bus);

        // Log VM topology on MMIO access.
        let nr = vcpu.vm.nr_vcpus();
        let this_pcpu = khal::percpu::this_cpu_id().as_usize();
        log::info!(
            "[VMM] MMIO {}: IPA={:#x} vcpu{} on pCPU{}",
            if is_write { "write" } else { "read" },
            ipa,
            vcpu.vcpu_id,
            this_pcpu,
        );
        for i in 0..nr as u32 {
            let pcpu = vcpu.vm.vcpu_pcpu(i);
            if pcpu >= 0 {
                log::info!("[VMM]   vcpu{} → pCPU{}", i, pcpu);
            }
        }

        // Advance past faulting instruction and resume.
        vcpu.arch.elr += 4;
        return ExitAction::Resume;
    }
    drop(bus);

    log::error!(
        "[VMM] Stage-2 {} fault: IPA={:#x} FAR={:#x} ELR={:#x}",
        if is_write { "write" } else { "read" },
        ipa,
        far,
        vcpu.arch.elr,
    );
    ExitAction::Exit
}

impl VmmArch for Aarch64Vhe {
    type ArchVcpu = Aarch64Vcpu;
    type GuestMem = crate::mm::stage2::Stage2;

    fn init_vcpu(vcpu: &mut Vcpu<Self>, entry: u64, sp: u64) -> bool {
        vcpu.arch.elr = kaddr_layout::v2p(entry as usize) as u64;
        vcpu.arch.sp_el1 = kaddr_layout::v2p(sp as usize) as u64;
        vcpu.arch.spsr = 0x5 | (0xF << 6); // EL1h, DAIF masked
        true
    }

    fn restore_guest_ctx(vcpu: &mut Vcpu<Self>) {
        // SAFETY: sysregs buffer is 128-byte aligned within Aarch64Vcpu.
        unsafe {
            kvmm_restore_sysregs_el12(vcpu.arch.sysregs.as_mut_ptr());
        }
    }

    fn enter_guest(vcpu: &mut Vcpu<Self>) -> bool {
        // SAFETY: el2_enter_guest saves/restores host state correctly.
        // It returns via el2_trap_exit when the guest traps.
        unsafe {
            el2_enter_guest(&mut vcpu.arch as *mut Aarch64Vcpu);
        }
        true
    }

    fn exit_handler(vcpu: &mut Vcpu<Self>) -> ExitAction {
        // Unmask IRQ — EL2 entry masks PSTATE.I; host needs interrupts.
        // SAFETY: re-enabling IRQ in host context is safe.
        unsafe { core::arch::asm!("msr daifclr, #2") };

        let exit_type = vcpu.arch.exit_type;

        if exit_type != 0 {
            // IRQ/FIQ/SError — not a synchronous exception.
            // ESR_EL2 is stale; just yield and resume guest.
            ktask::yield_now();
            return ExitAction::Resume;
        }

        // Synchronous exception — ESR_EL2 is valid.
        let esr = read_esr_el2();
        let ec = (esr >> 26) as u32;

        match ec {
            EC_WFI_WFE => handle_wfi(&mut vcpu.arch, esr),
            EC_HVC64 => handle_hvc(&mut vcpu.arch),
            EC_DATA_ABORT_LOWER => handle_data_abort(vcpu, esr),
            _ => {
                let far = read_far_el2();
                log::warn!(
                    "[VMM] Unhandled sync EC={:#x} ESR={:#x} FAR={:#x} ELR={:#x}",
                    ec,
                    esr,
                    far,
                    vcpu.arch.elr,
                );
                ExitAction::Exit
            }
        }
    }

    fn save_guest_ctx(vcpu: &mut Vcpu<Self>) {
        // SAFETY: sysregs buffer is 128-byte aligned within Aarch64Vcpu.
        unsafe {
            kvmm_save_sysregs_el12(vcpu.arch.sysregs.as_mut_ptr());
        }
    }

    fn guest_test_code() -> (*const u8, usize) {
        let start = kvmm_guest_test_entry as *const u8;
        let end = kvmm_guest_test_entry_end as *const u8;
        (start, end as usize - start as usize)
    }
}
