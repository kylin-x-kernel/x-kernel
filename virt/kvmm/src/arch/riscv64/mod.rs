// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V H-extension VMM implementation.
//!
//! The kernel runs in HS-mode (S-mode with H-extension). Guest vCPUs
//! execute in VS-mode via `sret`, trapping back on `ecall` or faults.

use super::VmmArch;
use crate::vcpu::{ExitAction, Vcpu};

core::arch::global_asm!(include_str!("hext_vcpu.S"));
core::arch::global_asm!(include_str!("guest_test.S"));

unsafe extern "C" {
    fn hext_enter_guest(vcpu: *mut RiscvVcpu) -> i32;
    pub(crate) fn kvmm_guest_test_entry_rv();
    pub(crate) fn kvmm_guest_test_entry_rv_end();
}

/// VS-mode ecall cause (scause = 10).
const CAUSE_VS_ECALL: u64 = 10;

/// Virtual instruction exception (scause = 22).
/// WFI in VS-mode with hstatus.VTW=1 traps here.
const CAUSE_VIRTUAL_INSTRUCTION: u64 = 22;

/// Guest hypercall numbers (a7 register).
const GUEST_ECALL_DONE: u64 = 0;
const GUEST_ECALL_PRINT: u64 = 1;

/// hstatus.VTW bit (trap WFI in VS-mode).
const HSTATUS_VTW: u64 = 1 << 21;

/// RISC-V H-extension VMM backend.
pub struct RiscvHext;

/// RISC-V vCPU state matching avatar-next `vcpu_t` layout.
#[repr(C)]
#[derive(Clone, Default)]
pub struct RiscvVcpu {
    /// x0-x31 guest GPRs (32 regs, offset 0).
    pub gprs: [u64; 32],
    /// Guest PC = vsepc (offset 256).
    pub vsepc: u64,
    /// Guest sstatus = vsstatus (offset 264).
    pub vsstatus: u64,
    /// Guest trap vector = vstvec (offset 272).
    pub vstvec: u64,
    /// Guest sscratch = vsscratch (offset 280).
    pub vsscratch: u64,
    /// Guest page table = vsatp (offset 288).
    pub vsatp: u64,
    /// Guest interrupt enable = vsie (offset 296).
    pub vsie: u64,
    /// Trap cause saved by HS-mode (offset 304).
    pub scause_save: u64,
    /// Trap value saved by HS-mode (offset 312).
    pub stval_save: u64,
    /// Stage-2 guest physical address (offset 320).
    pub htval_save: u64,
    /// Host context: ra, s0-s11, sp, orig_stvec, gp (offset 328).
    pub host_ctx: [u64; 16],
}

#[inline]
fn read_hstatus() -> u64 {
    let val: u64;
    // SAFETY: reading hstatus CSR is safe from HS-mode.
    unsafe { core::arch::asm!("csrr {}, 0x600", out(reg) val) };
    val
}

#[inline]
fn write_hstatus(val: u64) {
    // SAFETY: writing hstatus CSR is safe from HS-mode.
    unsafe { core::arch::asm!("csrw 0x600, {}", in(reg) val) };
}

/// Initialize H-extension: set hstatus.VTW to trap WFI in VS-mode.
pub fn hext_init() {
    let hs = read_hstatus();
    write_hstatus(hs | HSTATUS_VTW);
    log::info!("[HEXT] hstatus.VTW set");
}
fn handle_wfi(vcpu: &mut RiscvVcpu) -> ExitAction {
    vcpu.vsepc += 4;
    ktask::sleep(core::time::Duration::from_millis(1));
    ExitAction::Resume
}

fn handle_vs_ecall(vcpu: &mut RiscvVcpu) -> ExitAction {
    let nr = vcpu.gprs[17]; // a7
    let _arg0 = vcpu.gprs[10]; // a0

    // Step past ecall instruction.
    vcpu.vsepc += 4;

    match nr {
        GUEST_ECALL_PRINT => {
            // if arg0.is_multiple_of(20) {
            //     log::info!("[HEXT] ECALL_PRINT: iter={} (riscv64)", arg0);
            // }
            ExitAction::Resume
        }
        GUEST_ECALL_DONE => {
            log::info!("[HEXT] ECALL_DONE: guest exiting (riscv64)");
            ExitAction::VmExit
        }
        _ => {
            log::warn!("[HEXT] Unknown ecall a7={} (riscv64)", nr);
            vcpu.gprs[10] = u64::MAX; // return -1 to guest
            ExitAction::Resume
        }
    }
}

impl VmmArch for RiscvHext {
    type ArchVcpu = RiscvVcpu;
    type GuestMem = crate::mm::gstage::GStage;

    fn init_vcpu(vcpu: &mut Vcpu<Self>, entry: u64, sp: u64) -> bool {
        vcpu.arch.vsepc = kaddr_layout::v2p(entry as usize) as u64;
        vcpu.arch.gprs[2] = kaddr_layout::v2p(sp as usize) as u64;
        true
    }

    fn restore_guest_ctx(_vcpu: &mut Vcpu<Self>) {
        // VS-CSRs are restored inside hext_enter_guest assembly.
    }

    fn enter_guest(vcpu: &mut Vcpu<Self>) -> bool {
        // SAFETY: hext_enter_guest saves/restores host state correctly.
        let ret = unsafe { hext_enter_guest(&mut vcpu.arch as *mut RiscvVcpu) };
        ret != 0
    }

    fn exit_handler(vcpu: &mut Vcpu<Self>) -> ExitAction {
        let cause = vcpu.arch.scause_save;
        let is_interrupt = (cause >> 63) != 0;
        let code = cause & !(1u64 << 63);

        if is_interrupt {
            // Briefly enable HS-mode interrupts so the kernel can service
            // the pending interrupt (timer, IPI, etc.). Trap cleared SIE;
            // without this the interrupt stays pending and guest re-entry
            // immediately traps again in an infinite loop.
            // SAFETY: toggling SIE in sstatus is safe; it briefly enables
            // interrupts so the kernel can service the pending one.
            unsafe {
                core::arch::asm!(
                    "csrsi sstatus, 0x2", // SIE = bit 1
                    "csrci sstatus, 0x2",
                );
            }
            ktask::yield_now();
            ExitAction::Resume
        } else {
            match code {
                CAUSE_VIRTUAL_INSTRUCTION => {
                    // WFI trapped by hstatus.VTW=1.
                    handle_wfi(&mut vcpu.arch)
                }
                CAUSE_VS_ECALL => handle_vs_ecall(&mut vcpu.arch),
                12 | 13 | 15 => {
                    // Page fault (shouldn't happen without vsatp).
                    log::error!(
                        "[HEXT] Page fault: cause={} vsepc={:#x} stval={:#x}",
                        code,
                        vcpu.arch.vsepc,
                        vcpu.arch.stval_save,
                    );
                    ExitAction::Exit
                }
                20 | 21 | 23 => {
                    let gpa = (vcpu.arch.htval_save << 2) | (vcpu.arch.stval_save & 0xFFF);
                    let is_write = code == 23 || code == 21;

                    let mut bus = vcpu.vm.mmio_bus().lock();
                    if let Some(_val) = bus.handle(gpa, is_write, 4, 0) {
                        drop(bus);

                        let nr = vcpu.vm.nr_vcpus();
                        let this_pcpu = khal::percpu::this_cpu_id().as_usize();
                        log::info!(
                            "[HEXT] MMIO {}: GPA={:#x} vcpu{} on pCPU{}",
                            if is_write { "write" } else { "read/fetch" },
                            gpa,
                            vcpu.vcpu_id,
                            this_pcpu,
                        );
                        for i in 0..nr as u32 {
                            let pcpu = vcpu.vm.vcpu_pcpu(i);
                            if pcpu >= 0 {
                                log::info!("[HEXT]   vcpu{} → pCPU{}", i, pcpu);
                            }
                        }

                        vcpu.arch.vsepc += 4;
                        return ExitAction::Resume;
                    }
                    drop(bus);

                    log::error!(
                        "[HEXT] G-stage {} fault: gpa={:#x} vsepc={:#x}",
                        if is_write { "write" } else { "read/fetch" },
                        gpa,
                        vcpu.arch.vsepc,
                    );
                    ExitAction::Exit
                }
                _ => {
                    log::error!(
                        "[HEXT] Unhandled exit: cause={} vsepc={:#x}",
                        code,
                        vcpu.arch.vsepc,
                    );
                    ExitAction::Exit
                }
            }
        }
    }

    fn save_guest_ctx(_vcpu: &mut Vcpu<Self>) {
        // VS-CSRs are saved inside hext_trap_vector assembly.
    }

    fn guest_test_code() -> (*const u8, usize) {
        let start = kvmm_guest_test_entry_rv as *const u8;
        let end = kvmm_guest_test_entry_rv_end as *const u8;
        (start, end as usize - start as usize)
    }

    fn percpu_hw_init() -> bool {
        hext_init();
        true
    }
}
