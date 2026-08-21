// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! AArch64 VHE (Virtualization Host Extensions) VMM implementation.
//!
//! The kernel runs at EL2 with `HCR_EL2.E2H=1`. Guest vCPUs execute
//! at EL1 via `eret`, trapping back to EL2 on WFI/HVC/Data Abort.

use aarch64_cpu::registers::{DAIF, ReadWriteable};

use super::VmmArch;
use crate::{
    vcpu::{ExitAction, Vcpu},
    vdev::aarch64::{vpsci, vtimer},
    vm::VcpuRunState,
};

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
const EC_SMC64: u32 = 0x17;
const EC_DATA_ABORT_LOWER: u32 = 0x24;

/// HVC hypercall numbers.
const HVC_DONE: u64 = 0;
const HVC_PRINT: u64 = 1;
const PSCI_SYSTEM_OFF: u64 = 0x8400_0008;

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
    /// ESR_EL2 captured by the guest vector before host handling can clobber it
    /// (offset 528).
    pub esr: u64,
    /// FAR_EL2 captured on guest exit (offset 536).
    pub far: u64,
    /// HPFAR_EL2 captured on guest exit (offset 544).
    pub hpfar: u64,
    /// Guest CNTV_CTL_EL02 saved on exit (offset 552).
    pub cntv_ctl: u64,
    /// Guest CNTV_CVAL_EL02 saved on exit (offset 560).
    pub cntv_cval: u64,
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
            esr: 0,
            far: 0,
            hpfar: 0,
            cntv_ctl: 0,
            cntv_cval: 0,
        }
    }
}

fn advance_pc(vcpu: &mut Aarch64Vcpu, esr: u64) {
    let step = if (esr >> 25) & 1 == 1 { 4 } else { 2 };
    vcpu.elr += step;
}

fn handle_wfi(vcpu: &mut Vcpu<Aarch64Vhe>, esr: u64) -> ExitAction {
    advance_pc(&mut vcpu.arch, esr);

    // Park the vCPU in an interruptible sleep so an injected virtual IRQ can
    // wake it early via `inject_irq` -> `interrupt_task`. Publishing the
    // WfiSleeping state is what lets `inject_irq` know a wake is needed.
    let deadline = khal::time::monotonic_time() + ktime_types::TimeSpan::from_millis(1);
    vcpu.vm
        .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::WfiSleeping);
    ktask::interruptible_sleep_until(deadline);
    vcpu.vm
        .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::HostHandlingExit);
    ExitAction::Resume
}

fn handle_psci_action(vcpu: &mut Vcpu<Aarch64Vhe>) -> Option<ExitAction> {
    let action = vpsci::handle_psci(&mut vcpu.arch.gprs)?;
    match action {
        vpsci::PsciAction::Continue => Some(ExitAction::Resume),
        vpsci::PsciAction::Shutdown => Some(ExitAction::VmExit),
        vpsci::PsciAction::CpuOn {
            target_cpu,
            entry_addr,
            context_id,
        } => {
            let code =
                crate::vcpu::power_on_secondary(&vcpu.vm, target_cpu, entry_addr, context_id);
            vcpu.arch.gprs[0] = code;
            Some(ExitAction::Resume)
        }
    }
}

fn handle_hvc(vcpu: &mut Vcpu<Aarch64Vhe>, esr: u64) -> ExitAction {
    let _ = esr;
    if let Some(action) = handle_psci_action(vcpu) {
        return action;
    }

    let no = vcpu.arch.gprs[0];
    match no {
        HVC_PRINT => {
            let _iter = vcpu.arch.gprs[1];
            // if _iter.is_multiple_of(20) {
            //     log::info!("[VMM] HVC_PRINT: iter={} (aarch64)", _iter);
            // }
            ExitAction::Resume
        }
        HVC_DONE => {
            let tests = vcpu.arch.gprs[1];
            log::info!("[VMM] HVC_DONE: {} tests complete (aarch64)", tests);
            ExitAction::VmExit
        }
        PSCI_SYSTEM_OFF => {
            log::info!("[VMM] PSCI_SYSTEM_OFF from guest (aarch64)");
            ExitAction::VmExit
        }
        _ => {
            log::warn!("[VMM] Unknown HVC no={} (aarch64)", no);
            ExitAction::Resume
        }
    }
}

fn handle_smc(vcpu: &mut Vcpu<Aarch64Vhe>, esr: u64) -> ExitAction {
    if let Some(action) = handle_psci_action(vcpu) {
        advance_pc(&mut vcpu.arch, esr);
        return action;
    }

    log::warn!("[VMM] Unknown SMC no={} (aarch64)", vcpu.arch.gprs[0]);
    vcpu.arch.gprs[0] = vpsci::PSCI_RET_NOT_SUPPORTED;
    advance_pc(&mut vcpu.arch, esr);
    ExitAction::Resume
}

fn handle_data_abort(vcpu: &mut Vcpu<Aarch64Vhe>, esr: u64) -> ExitAction {
    // FAR_EL2/HPFAR_EL2 were captured by the world-switch exit stub before the
    // host unmasked IRQs, so they are not clobbered by later host exceptions.
    let far = vcpu.arch.far;
    let hpfar = vcpu.arch.hpfar;
    let ipa = (hpfar << 8) | (far & 0xFFF);

    // Decode the Data Abort ISS to move the actual data (ESR_EL2, EC=0x24):
    //   ISV [24] syndrome valid, SAS [23:22] access size, SRT [20:16] GPR,
    //   WnR [6] write-not-read.
    let isv = (esr >> 24) & 1;
    let sas = (esr >> 22) & 0x3;
    let srt = ((esr >> 16) & 0x1f) as usize;
    let is_write = (esr >> 6) & 1 != 0;
    let size: u8 = 1 << sas;

    if isv == 0 {
        log::error!(
            "[VMM] data abort without ISV: IPA={:#x} ESR={:#x} ELR={:#x}",
            ipa,
            esr,
            vcpu.arch.elr,
        );
        return ExitAction::Exit;
    }

    let mut bus = vcpu.vm.mmio_bus().lock();
    if is_write {
        // Source register value (x31 reads as zero).
        let val = if srt == 31 { 0 } else { vcpu.arch.gprs[srt] };
        if bus
            .handle_for_vcpu(ipa, true, size, val, vcpu.vcpu_id)
            .is_some()
        {
            drop(bus);
            vcpu.arch.elr += 4;
            return ExitAction::Resume;
        }
    } else if let Some(val) = bus.handle_for_vcpu(ipa, false, size, 0, vcpu.vcpu_id) {
        drop(bus);
        // Write the (zero-extended) result back into the destination GPR.
        if srt != 31 {
            vcpu.arch.gprs[srt] = val;
        }
        vcpu.arch.elr += 4;
        return ExitAction::Resume;
    }
    drop(bus);

    // unmapped MMIO
    if !is_write && srt != 31 {
        vcpu.arch.gprs[srt] = 0;
    }
    vcpu.arch.elr += 4;
    ExitAction::Resume
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

    fn init_secondary_vcpu(vcpu: &mut Vcpu<Self>, entry_pa: u64, context_id: u64) -> bool {
        vcpu.arch.elr = entry_pa;
        vcpu.arch.sp_el1 = 0;
        vcpu.arch.spsr = 0x5 | (0xF << 6); // EL1h, DAIF masked
        vcpu.arch.gprs[0] = context_id;
        true
    }

    fn restore_guest_ctx(vcpu: &mut Vcpu<Self>) {
        let vmpidr = (1u64 << 31) | vcpu.vcpu_id as u64;
        // SAFETY: VMPIDR_EL2 is an EL2-controlled virtualization register.
        unsafe {
            core::arch::asm!("msr vmpidr_el2, {}", in(reg) vmpidr);
        }
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
        // `modify` touches only the DAIF.I bit, so the D/A/F mask bits
        // are preserved.
        DAIF.modify(DAIF::I::Unmasked);

        // Timer delivery is recomputed from this vCPU's saved CNTV state on
        // each entry, avoiding per-CPU host-IRQ attribution between VMs.

        let exit_type = vcpu.arch.exit_type;

        if exit_type != 0 {
            // IRQ/FIQ/SError — not a synchronous exception. The host IRQ was
            // already serviced when we unmasked above; just re-enter the guest.
            // We do NOT yield here (that would deschedule on every host-timer
            // preemption and throttle the guest); host fairness is handled by
            // the run loop's bounded periodic yield.
            vcpu.exit_category = crate::vm::EXIT_CAT_INTERRUPT;
            return ExitAction::Resume;
        }

        // Synchronous exception — ESR_EL2 was captured by the guest vector
        // before the host unmasked IRQs, so it is valid here.
        let esr = vcpu.arch.esr;
        let ec = (esr >> 26) as u32;

        match ec {
            EC_WFI_WFE => {
                vcpu.exit_category = crate::vm::EXIT_CAT_HALT;
                handle_wfi(vcpu, esr)
            }
            EC_HVC64 => {
                vcpu.exit_category = crate::vm::EXIT_CAT_HYPERCALL;
                handle_hvc(vcpu, esr)
            }
            EC_SMC64 => {
                vcpu.exit_category = crate::vm::EXIT_CAT_HYPERCALL;
                handle_smc(vcpu, esr)
            }
            EC_DATA_ABORT_LOWER => {
                vcpu.exit_category = crate::vm::EXIT_CAT_MMIO;
                handle_data_abort(vcpu, esr)
            }
            _ => {
                vcpu.exit_category = crate::vm::EXIT_CAT_OTHER;
                let far = vcpu.arch.far;
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

    fn teardown_vcpu(_vcpu: &mut Vcpu<Self>) {
        vtimer::clear_host_vtimer_owner_for_current_task();
    }

    fn guest_test_code() -> (*const u8, usize) {
        let start = kvmm_guest_test_entry as *const u8;
        let end = kvmm_guest_test_entry_end as *const u8;
        (start, end as usize - start as usize)
    }
}
