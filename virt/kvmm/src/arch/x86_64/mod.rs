// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! x86_64 VMX (Intel VT-x) VMM implementation.
//!
//! Uses VMXON/VMCS/vmlaunch/vmresume for guest execution.
//! Exit handler dispatches by VM-exit reason from the VMCS.

use super::VmmArch;
use crate::vcpu::{ExitAction, Vcpu};

pub mod vmx;

use vmx::{
    VMX_HYPERCALL_DONE, VMX_HYPERCALL_PRINT, VMX_REASON_CPUID, VMX_REASON_EXC_NMI, VMX_REASON_HLT,
    VMX_REASON_VMCALL, VmcsField, vmcs_read,
};

core::arch::global_asm!(include_str!("vmx_run.S"), options(att_syntax));
core::arch::global_asm!(include_str!("guest_test.S"), options(att_syntax));

unsafe extern "C" {
    fn vmx_enter_guest(vcpu: *mut X86Vcpu) -> i32;
    pub(crate) fn kvmm_guest_test_entry_x86();
}

/// x86_64 VMX VMM backend.
pub struct X86Vmx;

/// x86_64 vCPU state matching avatar-next `vcpu_t` layout.
#[repr(C)]
pub struct X86Vcpu {
    /// Guest general-purpose registers at fixed offsets for assembly.
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rbp: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rflags: u64,
    /// Per-vCPU VMCS physical address (offset 0x80).
    pub vmcs_pa: u64,
}

impl Default for X86Vcpu {
    fn default() -> Self {
        Self {
            rax: 0,
            rbx: 0,
            rcx: 0,
            rdx: 0,
            rbp: 0,
            rsi: 0,
            rdi: 0,
            r8: 0,
            r9: 0,
            r10: 0,
            r11: 0,
            r12: 0,
            r13: 0,
            r14: 0,
            r15: 0,
            rflags: 0x0000_0002, // Reserved bit 1 always set
            vmcs_pa: 0,
        }
    }
}

fn handle_hlt(_vcpu: &mut X86Vcpu) -> ExitAction {
    let inst_len = vmcs_read(VmcsField::ExiInstLen);
    let rip = vmcs_read(VmcsField::GuestRip);
    vmx::vmcs_write(VmcsField::GuestRip, rip + inst_len);
    ktask::yield_now();
    ExitAction::Resume
}

fn handle_vmcall(vcpu: &mut X86Vcpu) -> ExitAction {
    let nr = vcpu.rax;
    let arg0 = vcpu.rdi;

    let inst_len = vmcs_read(VmcsField::ExiInstLen);
    let rip = vmcs_read(VmcsField::GuestRip);
    vmx::vmcs_write(VmcsField::GuestRip, rip + inst_len);

    match nr {
        VMX_HYPERCALL_PRINT => {
            if arg0.is_multiple_of(20) {
                log::info!("[VMX] VMCALL_PRINT: iter={} (x86_64)", arg0);
            }
            ExitAction::Resume
        }
        VMX_HYPERCALL_DONE => {
            log::info!("[VMX] VMCALL_DONE: guest exiting (x86_64)");
            ExitAction::VmExit
        }
        _ => {
            log::warn!("[VMX] Unknown vmcall rax={:#x} (x86_64)", nr);
            vcpu.rax = u64::MAX;
            ExitAction::Resume
        }
    }
}

fn handle_cpuid(vcpu: &mut X86Vcpu) -> ExitAction {
    let inst_len = vmcs_read(VmcsField::ExiInstLen);
    let rip = vmcs_read(VmcsField::GuestRip);
    vmx::vmcs_write(VmcsField::GuestRip, rip + inst_len);

    // Return zeros for all CPUID leaves (minimal emulation).
    vcpu.rax = 0;
    vcpu.rbx = 0;
    vcpu.rcx = 0;
    vcpu.rdx = 0;
    ExitAction::Resume
}

impl VmmArch for X86Vmx {
    type ArchVcpu = X86Vcpu;

    fn init_vcpu(vcpu: &mut Vcpu<Self>, entry: u64, sp: u64) -> bool {
        vmx::vmcs_init_vcpu(vcpu, entry, sp)
    }

    fn restore_guest_ctx(_vcpu: &mut Vcpu<Self>) {
        // VMCS auto-loads guest state on vmentry; no explicit restore needed.
    }

    fn enter_guest(vcpu: &mut Vcpu<Self>) -> bool {
        if !vmx::vmptrld(vcpu.arch.vmcs_pa) {
            log::error!("[VMX] vmptrld failed in enter_guest");
            return false;
        }
        // SAFETY: vmx_enter_guest saves/restores host callee-saved correctly.
        let ret = unsafe { vmx_enter_guest(&mut vcpu.arch as *mut X86Vcpu) };
        ret != 0
    }

    fn exit_handler(vcpu: &mut Vcpu<Self>) -> ExitAction {
        let reason = vmcs_read(VmcsField::ExiReason) as u32 & 0xFFFF;

        match reason {
            VMX_REASON_HLT => handle_hlt(&mut vcpu.arch),
            VMX_REASON_VMCALL => handle_vmcall(&mut vcpu.arch),
            VMX_REASON_CPUID => handle_cpuid(&mut vcpu.arch),
            VMX_REASON_EXC_NMI => {
                let intr_info = vmcs_read(VmcsField::ExiIntrInfo);
                log::error!(
                    "[VMX] Exception/NMI: intr_info={:#x} rip={:#x}",
                    intr_info,
                    vmcs_read(VmcsField::GuestRip),
                );
                ExitAction::Exit
            }
            _ => {
                log::error!(
                    "[VMX] Unhandled exit: reason={} rip={:#x} qual={:#x}",
                    reason,
                    vmcs_read(VmcsField::GuestRip),
                    vmcs_read(VmcsField::ExiQualification),
                );
                ExitAction::Exit
            }
        }
    }

    fn save_guest_ctx(_vcpu: &mut Vcpu<Self>) {
        // VMCS auto-saves guest state on vmexit; GPRs saved in assembly.
    }

    fn guest_test_entry() -> u64 {
        kvmm_guest_test_entry_x86 as *const () as u64
    }

    fn percpu_hw_init() -> bool {
        if !vmx::vmx_check_support() {
            log::error!("[VMX] VMX not supported");
            return false;
        }
        vmx::vmx_percpu_init()
    }
}
