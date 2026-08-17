// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RISC-V H-extension VMM implementation.
//!
//! The kernel runs in HS-mode (S-mode with H-extension). Guest vCPUs
//! execute in VS-mode via `sret`, trapping back on `ecall` or faults.

use super::VmmArch;
use crate::{
    mm::GuestMem,
    vcpu::{ExitAction, Vcpu},
};

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
const GUEST_ECALL_DONE: u64 = 0x584b_0000;
const GUEST_ECALL_PRINT: u64 = 0x584b_0001;

const SBI_SUCCESS: u64 = 0;
const SBI_ERR_NOT_SUPPORTED: u64 = (-2i64) as u64;
const SBI_LEGACY_SET_TIMER: u64 = 0x00;
const SBI_LEGACY_CONSOLE_PUTCHAR: u64 = 0x01;
const SBI_LEGACY_CONSOLE_GETCHAR: u64 = 0x02;
const SBI_EXT_BASE: u64 = 0x10;
const SBI_BASE_GET_SPEC_VERSION: u64 = 0;
const SBI_BASE_GET_IMPL_ID: u64 = 1;
const SBI_BASE_GET_IMPL_VERSION: u64 = 2;
const SBI_BASE_PROBE_EXTENSION: u64 = 3;
const SBI_EXT_TIME: u64 = 0x5449_4d45;
const SBI_EXT_RFENCE: u64 = 0x5246_4e43;
const SBI_TIME_SET_TIMER: u64 = 0;

/// hstatus.VTW bit (trap WFI in VS-mode).
const HSTATUS_VTW: u64 = 1 << 21;
const HSTATUS_SPVP: u64 = 1 << 8;
const HIE_VSEIE: u64 = 1 << 10;
const HIE_VSTIE: u64 = 1 << 6;
const HVIP_VSEIP: u64 = 1 << 10;
const HVIP_VSTIP: u64 = 1 << 6;
const HCOUNTEREN_CY_TM_IR: u64 = 0x7;
const HEDELEG_GUEST_EXCEPTIONS: u64 = (1 << 3) | (1 << 8) | (1 << 12) | (1 << 13) | (1 << 15);
const CSR_HEDELEG: u16 = 0x602;
const CSR_HIDELEG: u16 = 0x603;
const CSR_HIE: u16 = 0x604;
const CSR_HCOUNTEREN: u16 = 0x606;
const CSR_HVIP: u16 = 0x645;

/// RISC-V H-extension VMM backend.
pub struct RiscvHext;

/// RISC-V vCPU state matching avatar-next `vcpu_t` layout.
#[repr(C)]
#[derive(Clone)]
pub struct RiscvVcpu {
    /// x0-x31 guest GPRs (32 regs, offset 0).
    pub gprs: [u64; 32],
    /// Guest resume PC used as HS `sepc` for `sret` into VS/VU-mode.
    pub pc: u64,
    /// Guest VS exception PC CSR.
    pub vsepc: u64,
    /// Guest sstatus = vsstatus (offset 272).
    pub vsstatus: u64,
    /// Guest trap vector = vstvec (offset 280).
    pub vstvec: u64,
    /// Guest sscratch = vsscratch (offset 288).
    pub vsscratch: u64,
    /// Guest page table = vsatp (offset 296).
    pub vsatp: u64,
    /// Guest interrupt enable = vsie (offset 304).
    pub vsie: u64,
    /// Trap cause saved by HS-mode (offset 312).
    pub scause_save: u64,
    /// Trap value saved by HS-mode (offset 320).
    pub stval_save: u64,
    /// Stage-2 guest physical address (offset 328).
    pub htval_save: u64,
    /// Trap instruction saved by HS-mode (offset 336).
    pub htinst_save: u64,
    /// hstatus snapshot carrying the guest's previous virtual privilege mode.
    pub hstatus: u64,
    /// Host context: ra, s0-s11, sp, orig_stvec, gp (offset 352).
    pub host_ctx: [u64; 16],
    /// Guest-programmed SBI timer deadline in guest time units.
    pub timer_deadline: u64,
    console_buf: [u8; 256],
    console_len: usize,
}

impl Default for RiscvVcpu {
    fn default() -> Self {
        Self {
            gprs: [0; 32],
            pc: 0,
            vsepc: 0,
            vsstatus: 0,
            vstvec: 0,
            vsscratch: 0,
            vsatp: 0,
            vsie: 0,
            scause_save: 0,
            stval_save: 0,
            htval_save: 0,
            htinst_save: 0,
            hstatus: HSTATUS_SPVP,
            host_ctx: [0; 16],
            timer_deadline: 0,
            console_buf: [0; 256],
            console_len: 0,
        }
    }
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

#[inline]
fn set_hvip_bits(mask: u64, pending: bool) {
    if pending {
        // SAFETY: setting `hvip` virtual pending bits is the H-extension IRQ
        // injection mechanism and does not access memory.
        unsafe { core::arch::asm!("csrs {}, {}", const CSR_HVIP, in(reg) mask) };
    } else {
        // SAFETY: clearing `hvip` virtual pending bits only affects virtual IRQ
        // injection state for the current pCPU.
        unsafe { core::arch::asm!("csrc {}, {}", const CSR_HVIP, in(reg) mask) };
    }
}

/// Set or clear VS external interrupt pending on the current pCPU.
pub(crate) fn set_virtual_external_irq_pending(pending: bool) {
    set_hvip_bits(HVIP_VSEIP, pending);
}

/// Set or clear VS timer interrupt pending on the current pCPU.
pub(crate) fn set_virtual_timer_irq_pending(pending: bool) {
    set_hvip_bits(HVIP_VSTIP, pending);
}

/// Read the RISC-V time counter used by guest `rdtime` and SBI timer deadlines.
pub(crate) fn read_time() -> u64 {
    let val: u64;
    // SAFETY: reading the time CSR is a side-effect-free counter read.
    unsafe { core::arch::asm!("csrr {}, time", out(reg) val) };
    val
}

/// Initialize H-extension: set hstatus.VTW to trap WFI in VS-mode.
pub fn hext_init() {
    let hs = read_hstatus();
    write_hstatus(hs | HSTATUS_VTW);
    // SAFETY: delegating and enabling VS external/timer interrupt injection is
    // per-pCPU H-extension setup and does not access memory.
    unsafe {
        core::arch::asm!(
            "csrs {}, {}",
            "csrs {}, {}",
            "csrs {}, {}",
            "csrs {}, {}",
            const CSR_HEDELEG,
            in(reg) HEDELEG_GUEST_EXCEPTIONS,
            const CSR_HIDELEG,
            in(reg) HIE_VSEIE | HIE_VSTIE,
            const CSR_HIE,
            in(reg) HIE_VSEIE | HIE_VSTIE,
            const CSR_HCOUNTEREN,
            in(reg) HCOUNTEREN_CY_TM_IR,
        );
    }
    // log::info!("[HEXT] hstatus.VTW set");
}
fn handle_wfi(vcpu: &mut RiscvVcpu) -> ExitAction {
    vcpu.pc += 4;
    ktask::sleep(ktime_types::TimeSpan::from_millis(1));
    ExitAction::Resume
}

fn handle_vs_ecall(vcpu: &mut RiscvVcpu) -> ExitAction {
    let ext = vcpu.gprs[17]; // a7
    let func = vcpu.gprs[16]; // a6
    let arg0 = vcpu.gprs[10]; // a0

    // Step past ecall instruction.
    vcpu.pc += 4;

    match ext {
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
        SBI_LEGACY_SET_TIMER => {
            vcpu.timer_deadline = arg0;
            ExitAction::Resume
        }
        SBI_LEGACY_CONSOLE_PUTCHAR => {
            sbi_console_putchar(vcpu, arg0 as u8);
            ExitAction::Resume
        }
        SBI_LEGACY_CONSOLE_GETCHAR => {
            vcpu.gprs[10] = u64::MAX;
            ExitAction::Resume
        }
        SBI_EXT_BASE => {
            let value = match func {
                SBI_BASE_GET_SPEC_VERSION => 0x0000_0002,
                SBI_BASE_GET_IMPL_ID => 0x584b_564d, // "XKVM"
                SBI_BASE_GET_IMPL_VERSION => 1,
                SBI_BASE_PROBE_EXTENSION => match arg0 {
                    SBI_EXT_BASE
                    | SBI_EXT_TIME
                    | SBI_EXT_RFENCE
                    | SBI_LEGACY_CONSOLE_PUTCHAR
                    | SBI_LEGACY_CONSOLE_GETCHAR => 1,
                    _ => 0,
                },
                _ => {
                    vcpu.gprs[10] = SBI_ERR_NOT_SUPPORTED;
                    vcpu.gprs[11] = 0;
                    return ExitAction::Resume;
                }
            };
            vcpu.gprs[10] = SBI_SUCCESS;
            vcpu.gprs[11] = value;
            ExitAction::Resume
        }
        SBI_EXT_TIME if func == SBI_TIME_SET_TIMER => {
            vcpu.timer_deadline = arg0;
            vcpu.gprs[10] = SBI_SUCCESS;
            vcpu.gprs[11] = 0;
            ExitAction::Resume
        }
        SBI_EXT_RFENCE => {
            // Single-vCPU VMs have no remote harts to synchronize. Linux still
            // probes and may call RFENCE, so report success for the no-op case.
            vcpu.gprs[10] = SBI_SUCCESS;
            vcpu.gprs[11] = 0;
            ExitAction::Resume
        }
        _ => {
            log::warn!(
                "[HEXT] Unknown SBI ecall ext={:#x} func={:#x} (riscv64)",
                ext,
                func
            );
            vcpu.gprs[10] = SBI_ERR_NOT_SUPPORTED;
            vcpu.gprs[11] = 0;
            ExitAction::Resume
        }
    }
}

fn sbi_console_putchar(vcpu: &mut RiscvVcpu, byte: u8) {
    if byte == b'\r' {
        return;
    }
    if byte == b'\n' {
        flush_sbi_console(vcpu);
        return;
    }
    if vcpu.console_len >= vcpu.console_buf.len() {
        flush_sbi_console(vcpu);
    }
    let pos = vcpu.console_len;
    vcpu.console_buf[pos] = byte;
    vcpu.console_len = pos + 1;
}

fn flush_sbi_console(vcpu: &mut RiscvVcpu) {
    if vcpu.console_len == 0 {
        return;
    }
    let line =
        core::str::from_utf8(&vcpu.console_buf[..vcpu.console_len]).unwrap_or("<invalid utf8>");
    log::info!("[guest rv64] {}", line);
    vcpu.console_len = 0;
}

#[derive(Clone, Copy)]
struct MmioAccess {
    is_write: bool,
    size: u8,
    reg: usize,
    inst_len: u64,
}

fn decode_mmio_access(inst: u64, is_store_fault: bool) -> Option<MmioAccess> {
    if inst & 0x3 != 0x3 {
        return decode_compressed_mmio_access(inst as u16, is_store_fault);
    }

    let inst = inst as u32;
    let opcode = inst & 0x7f;
    let funct3 = (inst >> 12) & 0x7;

    match opcode {
        0x03 if !is_store_fault => {
            let size = match funct3 {
                0 | 4 => 1,
                1 | 5 => 2,
                2 | 6 => 4,
                3 => 8,
                _ => return None,
            };
            Some(MmioAccess {
                is_write: false,
                size,
                reg: ((inst >> 7) & 0x1f) as usize,
                inst_len: 4,
            })
        }
        0x23 if is_store_fault => {
            let size = match funct3 {
                0 => 1,
                1 => 2,
                2 => 4,
                3 => 8,
                _ => return None,
            };
            Some(MmioAccess {
                is_write: true,
                size,
                reg: ((inst >> 20) & 0x1f) as usize,
                inst_len: 4,
            })
        }
        _ => None,
    }
}

fn decode_compressed_mmio_access(inst: u16, is_store_fault: bool) -> Option<MmioAccess> {
    let opcode = inst & 0x3;
    let funct3 = (inst >> 13) & 0x7;

    match (opcode, funct3) {
        (0, 2 | 3) if !is_store_fault => Some(MmioAccess {
            is_write: false,
            size: if funct3 == 2 { 4 } else { 8 },
            reg: (((inst >> 2) & 0x7) + 8) as usize,
            inst_len: 2,
        }),
        (0, 6 | 7) if is_store_fault => Some(MmioAccess {
            is_write: true,
            size: if funct3 == 6 { 4 } else { 8 },
            reg: (((inst >> 2) & 0x7) + 8) as usize,
            inst_len: 2,
        }),
        (2, 2 | 3) if !is_store_fault => Some(MmioAccess {
            is_write: false,
            size: if funct3 == 2 { 4 } else { 8 },
            reg: ((inst >> 7) & 0x1f) as usize,
            inst_len: 2,
        }),
        (2, 6 | 7) if is_store_fault => Some(MmioAccess {
            is_write: true,
            size: if funct3 == 6 { 4 } else { 8 },
            reg: ((inst >> 2) & 0x1f) as usize,
            inst_len: 2,
        }),
        _ => None,
    }
}

fn read_guest_u16(vcpu: &Vcpu<RiscvHext>, gpa: u64) -> Option<u16> {
    let guest_mem = vcpu.vm.guest_mem()?;
    let hpa = guest_mem.gpa_to_hpa(gpa)?;
    let src = kaddr_layout::p2v(hpa as usize) as *const u16;
    // SAFETY: `hpa` came from a valid guest RAM translation and the read is
    // bounded to one unaligned instruction halfword.
    Some(unsafe { core::ptr::read_unaligned(src) })
}

fn read_guest_u32(vcpu: &Vcpu<RiscvHext>, gpa: u64) -> Option<u32> {
    let guest_mem = vcpu.vm.guest_mem()?;
    let hpa = guest_mem.gpa_to_hpa(gpa)?;
    let src = kaddr_layout::p2v(hpa as usize) as *const u32;
    // SAFETY: `hpa` came from the VM's valid guest RAM translation and this
    // reads exactly one naturally aligned instruction word for MMIO decode.
    Some(unsafe { core::ptr::read_unaligned(src) })
}

fn read_guest_u64(vcpu: &Vcpu<RiscvHext>, gpa: u64) -> Option<u64> {
    let guest_mem = vcpu.vm.guest_mem()?;
    let hpa = guest_mem.gpa_to_hpa(gpa)?;
    let src = kaddr_layout::p2v(hpa as usize) as *const u64;
    // SAFETY: `hpa` came from a valid guest RAM translation and the read is
    // bounded to one unaligned page-table entry.
    Some(unsafe { core::ptr::read_unaligned(src) })
}

fn guest_va_to_gpa(vcpu: &Vcpu<RiscvHext>, va: u64) -> Option<u64> {
    const SATP_MODE_SV39: u64 = 8;
    const SATP_MODE_SV48: u64 = 9;
    const SATP_MODE_SV57: u64 = 10;
    const PTE_V: u64 = 1 << 0;
    const PTE_R: u64 = 1 << 1;
    const PTE_W: u64 = 1 << 2;
    const PTE_X: u64 = 1 << 3;

    let satp = vcpu.arch.vsatp;
    let levels = match satp >> 60 {
        SATP_MODE_SV39 => 3,
        SATP_MODE_SV48 => 4,
        SATP_MODE_SV57 => 5,
        _ => return Some(va),
    };
    let va_bits = 12 + levels * 9;
    let sign_bit = 1u64 << (va_bits - 1);
    if va & sign_bit != 0 {
        let sign_extend_mask = !((1u64 << va_bits) - 1);
        if va & sign_extend_mask != sign_extend_mask {
            return None;
        }
    } else if va >> va_bits != 0 {
        return None;
    }

    let vpn_offset = if va_bits == 64 {
        va
    } else {
        va & ((1u64 << va_bits) - 1)
    };
    if satp == 0 {
        return Some(va);
    }

    let mut table_gpa = (satp & ((1u64 << 44) - 1)) << 12;
    for level in (0..levels).rev() {
        let vpn = (vpn_offset >> (12 + level * 9)) & 0x1ff;
        let pte = read_guest_u64(vcpu, table_gpa + vpn * 8)?;
        if pte & PTE_V == 0 || (pte & PTE_W != 0 && pte & PTE_R == 0) {
            return None;
        }
        if pte & (PTE_R | PTE_X) != 0 {
            let ppn = (pte >> 10) & ((1u64 << 44) - 1);
            let page_offset_mask = (1u64 << (12 + level * 9)) - 1;
            return Some((ppn << 12) | (vpn_offset & page_offset_mask));
        }
        table_gpa = ((pte >> 10) & ((1u64 << 44) - 1)) << 12;
    }
    None
}

fn mmio_instruction(vcpu: &Vcpu<RiscvHext>) -> Option<u64> {
    if let Some(pc_gpa) = guest_va_to_gpa(vcpu, vcpu.arch.pc)
        && let Some(lo) = read_guest_u16(vcpu, pc_gpa)
    {
        if lo & 0x3 != 0x3 {
            return Some(lo as u64);
        }
        if let Some(inst) = read_guest_u32(vcpu, pc_gpa) {
            return Some(inst as u64);
        }
    }
    if vcpu.arch.htinst_save != 0 {
        Some(vcpu.arch.htinst_save)
    } else {
        None
    }
}

impl VmmArch for RiscvHext {
    type ArchVcpu = RiscvVcpu;
    type GuestMem = crate::mm::gstage::GStage;

    fn init_vcpu(vcpu: &mut Vcpu<Self>, entry: u64, sp: u64) -> bool {
        vcpu.arch.pc = kaddr_layout::v2p(entry as usize) as u64;
        vcpu.arch.gprs[2] = kaddr_layout::v2p(sp as usize) as u64;
        true
    }

    fn restore_guest_ctx(_vcpu: &mut Vcpu<Self>) {
        // VS-CSRs are restored inside hext_enter_guest assembly.
    }

    fn enter_guest(vcpu: &mut Vcpu<Self>) -> bool {
        if !Self::percpu_hw_init() {
            log::error!("[HEXT] percpu_hw_init failed in enter_guest");
            return false;
        }
        // SAFETY: hext_enter_guest saves/restores host state correctly.
        let ret = unsafe { hext_enter_guest(&mut vcpu.arch as *mut RiscvVcpu) };
        ret != 0
    }

    fn exit_handler(vcpu: &mut Vcpu<Self>) -> ExitAction {
        let cause = vcpu.arch.scause_save;
        let is_interrupt = (cause >> 63) != 0;
        let code = cause & !(1u64 << 63);

        if is_interrupt {
            vcpu.exit_category = crate::vm::EXIT_CAT_INTERRUPT;
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
                    vcpu.exit_category = crate::vm::EXIT_CAT_HALT;
                    handle_wfi(&mut vcpu.arch)
                }
                CAUSE_VS_ECALL => {
                    vcpu.exit_category = crate::vm::EXIT_CAT_HYPERCALL;
                    handle_vs_ecall(&mut vcpu.arch)
                }
                12 | 13 | 15 => {
                    // Page fault (shouldn't happen without vsatp).
                    vcpu.exit_category = crate::vm::EXIT_CAT_OTHER;
                    log::error!(
                        "[HEXT] Page fault: cause={} pc={:#x} stval={:#x}",
                        code,
                        vcpu.arch.pc,
                        vcpu.arch.stval_save,
                    );
                    ExitAction::Exit
                }
                20 | 21 | 23 => {
                    vcpu.exit_category = crate::vm::EXIT_CAT_MMIO;
                    let gpa = (vcpu.arch.htval_save << 2) | (vcpu.arch.stval_save & 0xFFF);
                    let Some(inst) = mmio_instruction(vcpu) else {
                        log::error!(
                            "[HEXT] failed to fetch MMIO instruction: pc={:#x} htinst={:#x}",
                            vcpu.arch.pc,
                            vcpu.arch.htinst_save,
                        );
                        return ExitAction::Exit;
                    };
                    let Some(access) = decode_mmio_access(inst, code == 23) else {
                        log::error!(
                            "[HEXT] unsupported MMIO instruction: inst={:#x} cause={} gpa={:#x} \
                             pc={:#x}",
                            inst,
                            code,
                            gpa,
                            vcpu.arch.pc,
                        );
                        return ExitAction::Exit;
                    };
                    let value = if access.is_write {
                        vcpu.arch.gprs[access.reg]
                    } else {
                        0
                    };

                    let mut bus = vcpu.vm.mmio_bus().lock();
                    if let Some(val) =
                        bus.handle_for_vcpu(gpa, access.is_write, access.size, value, vcpu.vcpu_id)
                    {
                        drop(bus);
                        if !access.is_write && access.reg != 0 {
                            vcpu.arch.gprs[access.reg] = val;
                        }

                        vcpu.arch.pc += access.inst_len;
                        return ExitAction::Resume;
                    }
                    drop(bus);

                    log::error!(
                        "[HEXT] G-stage {} fault: gpa={:#x} pc={:#x}",
                        if access.is_write {
                            "write"
                        } else {
                            "read/fetch"
                        },
                        gpa,
                        vcpu.arch.pc,
                    );
                    ExitAction::Exit
                }
                _ => {
                    vcpu.exit_category = crate::vm::EXIT_CAT_OTHER;
                    log::error!(
                        "[HEXT] Unhandled exit: cause={} pc={:#x}",
                        code,
                        vcpu.arch.pc,
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
