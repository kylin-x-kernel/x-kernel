// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! riscv64 half of the `/dev/kvm` compatibility device.
//!
//! Owns everything that depends on the guest's KVM register ABI, exception
//! encoding and device model. The shared fd plumbing in the parent module calls
//! into the same set of names as [`super::aarch64`] does, so the parent stays
//! architecture-independent.
//!
//! lkvm's riscv64 build runs a userspace irqchip: it tries
//! `KVM_CREATE_DEVICE(KVM_DEV_TYPE_RISCV_AIA)`, and because the parent module
//! keeps that ioctl failing, it falls back to emulating the PLIC itself. The
//! consequences shape this module:
//!
//! * no device is registered on the kernel's MMIO bus, so the PLIC, UART and
//!   virtio windows stay unmapped in the G-stage and fault out to lkvm as
//!   `KVM_EXIT_MMIO`;
//! * the supervisor external interrupt is driven by `KVM_INTERRUPT` rather than
//!   by an in-kernel interrupt controller;
//! * lkvm never touches the guest PC, so this module advances it on every exit
//!   it handles.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use kvfs::{VfsError, VfsResult};
use kvmm::{
    Vcpu, Vm,
    arch::{CurrentArch, riscv64 as rv},
    vcpu_state::VcpuRunState,
};
use posix_types::{UserConstPtr, UserPtr};

use super::{DtbPatch, KVM_RUN_EXIT_REASON_OFF, KvmVcpuFile, KvmVmFile, MmioExit};

// vCPU-fd ioctls.
const KVM_INTERRUPT: u32 = 0x4004_ae86;
const KVM_GET_MP_STATE: u32 = 0x8004_ae98;

/// Sentinel arguments for `KVM_INTERRUPT`, not interrupt numbers.
const KVM_INTERRUPT_SET: u32 = 0xffff_ffff;
const KVM_INTERRUPT_UNSET: u32 = 0xffff_fffe;

const KVM_MP_STATE_RUNNABLE: u32 = 0;

/// Guest exception causes this module decodes.
const CAUSE_VS_ECALL: u64 = 10;
const CAUSE_INST_GUEST_PAGE_FAULT: u64 = 20;
const CAUSE_LOAD_GUEST_PAGE_FAULT: u64 = 21;
const CAUSE_VIRTUAL_INSTRUCTION: u64 = 22;
const CAUSE_STORE_GUEST_PAGE_FAULT: u64 = 23;

// `KVM_REG_RISCV` encoding: bit 63 marks the architecture, bits 52..59 hold the
// register size class, bits 24..31 the type, bits 16..23 the subtype and bits
// 0..15 the index. See kvmtool's `riscv/include/asm/kvm.h`.
const KVM_REG_RISCV: u64 = 0x8000_0000_0000_0000;
const KVM_REG_SIZE_MASK: u64 = 0x00f0_0000_0000_0000;
const KVM_REG_SIZE_U64: u64 = 0x0030_0000_0000_0000;
const KVM_REG_RISCV_TYPE_MASK: u64 = 0xff << 24;
const KVM_REG_RISCV_SUBTYPE_MASK: u64 = 0xff << 16;
const KVM_REG_RISCV_IDX_MASK: u64 = 0xffff;

const KVM_REG_RISCV_CONFIG: u64 = 0x01 << 24;
const KVM_REG_RISCV_CORE: u64 = 0x02 << 24;
const KVM_REG_RISCV_CSR: u64 = 0x03 << 24;
const KVM_REG_RISCV_TIMER: u64 = 0x04 << 24;
const KVM_REG_RISCV_ISA_EXT: u64 = 0x07 << 24;
const KVM_REG_RISCV_SBI_EXT: u64 = 0x08 << 24;

const KVM_REG_RISCV_SINGLE: u64 = 0;

// `struct kvm_riscv_config` field indices.
const CONFIG_ISA: u64 = 0;
const CONFIG_ZICBOM_BLOCK_SIZE: u64 = 1;
const CONFIG_MVENDORID: u64 = 2;
const CONFIG_MARCHID: u64 = 3;
const CONFIG_MIMPID: u64 = 4;
const CONFIG_ZICBOZ_BLOCK_SIZE: u64 = 5;
const CONFIG_SATP_MODE: u64 = 6;
const CONFIG_ZICBOP_BLOCK_SIZE: u64 = 7;

/// Sv39, the mode the G-stage tables and the guest's own page tables use.
const SATP_MODE_SV39: u64 = 8;

// `struct kvm_riscv_core` field indices: `struct user_regs_struct` order, so
// index 0 is the PC and indices 1..=31 are x1..x31.
const CORE_PC: u64 = 0;
const CORE_MODE: u64 = 32;
const KVM_RISCV_MODE_U: u64 = 0;
const KVM_RISCV_MODE_S: u64 = 1;

// `struct kvm_riscv_csr` field indices.
const CSR_SSTATUS: u64 = 0;
const CSR_SIE: u64 = 1;
const CSR_STVEC: u64 = 2;
const CSR_SSCRATCH: u64 = 3;
const CSR_SEPC: u64 = 4;
const CSR_SCAUSE: u64 = 5;
const CSR_STVAL: u64 = 6;
const CSR_SIP: u64 = 7;
const CSR_SATP: u64 = 8;
const CSR_SCOUNTEREN: u64 = 9;
const CSR_SENVCFG: u64 = 10;

// `struct kvm_riscv_timer` field indices.
const TIMER_FREQUENCY: u64 = 0;
const TIMER_TIME: u64 = 1;
const TIMER_COMPARE: u64 = 2;
const TIMER_STATE: u64 = 3;
const KVM_RISCV_TIMER_STATE_OFF: u64 = 0;
const KVM_RISCV_TIMER_STATE_ON: u64 = 1;

// RISC-V ISA extension ids, from Linux's `enum KVM_RISCV_ISA_EXT_ID`.
const ISA_EXT_A: u64 = 0;
const ISA_EXT_C: u64 = 1;
const ISA_EXT_D: u64 = 2;
const ISA_EXT_F: u64 = 3;
const ISA_EXT_I: u64 = 5;
const ISA_EXT_M: u64 = 6;
const ISA_EXT_ZICNTR: u64 = 19;
const ISA_EXT_ZICSR: u64 = 20;
const ISA_EXT_ZIFENCEI: u64 = 21;
const ISA_EXT_ZIHPM: u64 = 22;
const ISA_EXT_MAX: u64 = 75;

// SBI extension ids, from Linux's `enum KVM_RISCV_SBI_EXT_ID`.
const SBI_EXT_V01: u64 = 0;
const SBI_EXT_TIME: u64 = 1;
const SBI_EXT_RFENCE: u64 = 3;
const SBI_EXT_SRST: u64 = 4;
const SBI_EXT_MAX: u64 = 14;

/// The ISA extensions this kernel reports to the guest.
///
/// lkvm turns each reported extension into the guest's `riscv,isa` string, so
/// this must not claim anything the host cannot execute or this kernel does not
/// implement. In particular `SSTC` would have the guest program `stimecmp`
/// (losing its timer), `SSAIA` would move lkvm onto an in-kernel irqchip path
/// that does not exist here, and `ZICBOM`/`ZICBOZ` would send the guest down
/// cache-block operations this kernel cannot decode.
const SUPPORTED_ISA_EXTS: &[u64] = &[
    ISA_EXT_A,
    ISA_EXT_C,
    ISA_EXT_D,
    ISA_EXT_F,
    ISA_EXT_I,
    ISA_EXT_M,
    ISA_EXT_ZICNTR,
    ISA_EXT_ZICSR,
    ISA_EXT_ZIFENCEI,
    ISA_EXT_ZIHPM,
];

/// The SBI extensions this kernel services in `handle_sbi_ecall`.
const SUPPORTED_SBI_EXTS: &[u64] = &[SBI_EXT_V01, SBI_EXT_TIME, SBI_EXT_RFENCE, SBI_EXT_SRST];

/// No riscv-specific capability is advertised; lkvm requires only
/// `KVM_CAP_ONE_REG`, which the parent module answers.
pub(super) fn check_extension(_cap: usize) -> usize {
    0
}

/// Whether hardware discards a write to guest register `reg`.
///
/// RISC-V's hardwired-zero register is x0; unlike AArch64 XZR, it is index 0.
pub(super) const fn mmio_reg_is_discarded(reg: usize) -> bool {
    reg == 0
}

/// Nothing to rewrite: a riscv guest has no `/psci` node, and lkvm writes its
/// own `console=`, `earlycon=` and `root=` into `bootargs`.
pub(super) const DTB_PATCHES: &[DtbPatch] = &[];

/// Empty: the riscv boot command line lkvm generates already carries the
/// console and earlycon settings this guest needs.
pub(super) const EXTRA_BOOTARGS: &[u8] = b"";

/// The guest's boot device tree is passed in a1 on riscv (a0 carries the hart
/// id). Reading x0 instead would silently find a permanently-zero register.
pub(super) fn guest_dtb_user_addr(vm_file: &KvmVmFile, vcpu: &Vcpu<CurrentArch>) -> Option<usize> {
    vm_file.guest_userspace_addr(vcpu.arch.gprs[11])
}

pub(super) fn log_run_start(vcpu: &Vcpu<CurrentArch>) {
    log::info!(
        "[kvm] KVM_RUN start vcpu{} pc={:#x} a0={:#x} a1={:#x}",
        vcpu.vcpu_id,
        vcpu.arch.pc,
        vcpu.arch.gprs[10],
        vcpu.arch.gprs[11],
    );
}

/// Drive the VS external interrupt line from the level `KVM_INTERRUPT` recorded.
///
/// `hvip` is a per-hart CSR, so this runs on the pCPU about to enter the guest
/// rather than at ioctl time, where the caller's task could migrate afterwards.
pub(super) fn apply_irq_state(file: &KvmVcpuFile, _vcpu: &mut Vcpu<CurrentArch>) {
    rv::set_virtual_external_irq_pending(file.external_irq_pending.load(Ordering::Relaxed));
}

pub(super) fn prepare_vm(vm: &mut Vm<CurrentArch>, _vm_file: &KvmVmFile) -> VfsResult<()> {
    use kvmm::vdev::riscv64::timer::RiscvTimerHookFactory;

    // The guest's timer arrives as an SBI TIME call, which only records a
    // deadline; this hook turns a due deadline into `hvip.VSTIP` on the way
    // into the guest. Without it the guest never takes a timer interrupt and
    // hangs partway through boot.
    //
    // Nothing is registered on the MMIO bus on purpose: lkvm emulates the PLIC,
    // the UART and its virtio devices in userspace, so those windows must stay
    // unmapped in the G-stage and fault out to it as KVM_EXIT_MMIO.
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(RiscvTimerHookFactory));
    log::info!("[kvm] riscv timer hook wired");
    Ok(())
}

/// No riscv ioctl is serviced at VM-fd level.
pub(super) fn vm_ioctl(
    _vm_file: &Arc<KvmVmFile>,
    _cmd: u32,
    _arg: usize,
) -> Option<VfsResult<usize>> {
    None
}

pub(super) fn vcpu_ioctl(
    vcpu_file: &KvmVcpuFile,
    cmd: u32,
    arg: usize,
) -> Option<VfsResult<usize>> {
    match cmd {
        KVM_INTERRUPT => Some(set_interrupt(vcpu_file, arg)),
        KVM_GET_MP_STATE => Some(get_mp_state(arg)),
        _ => None,
    }
}

fn set_interrupt(vcpu_file: &KvmVcpuFile, arg: usize) -> VfsResult<usize> {
    let request = UserConstPtr::<u32>::from(arg).read_vm()?;
    match request {
        KVM_INTERRUPT_SET => vcpu_file
            .external_irq_pending
            .store(true, Ordering::Relaxed),
        KVM_INTERRUPT_UNSET => vcpu_file
            .external_irq_pending
            .store(false, Ordering::Relaxed),
        _ => return Err(VfsError::InvalidInput),
    }
    Ok(0)
}

fn get_mp_state(arg: usize) -> VfsResult<usize> {
    // Never report `KVM_MP_STATE_STOPPED`: lkvm reads that as "the kernel will
    // bring this hart up itself" and skips setting pc/a0/a1, which this path
    // does not implement.
    UserPtr::<u32>::from(arg).write_vm(KVM_MP_STATE_RUNNABLE)?;
    Ok(0)
}

/// Decoded `KVM_REG_RISCV` register id.
struct RiscvReg {
    ty: u64,
    subtype: u64,
    idx: u64,
}

impl RiscvReg {
    fn decode(id: u64) -> VfsResult<Self> {
        if id & KVM_REG_RISCV != KVM_REG_RISCV || id & KVM_REG_SIZE_MASK != KVM_REG_SIZE_U64 {
            return Err(VfsError::InvalidInput);
        }
        Ok(Self {
            ty: id & KVM_REG_RISCV_TYPE_MASK,
            subtype: id & KVM_REG_RISCV_SUBTYPE_MASK,
            idx: id & KVM_REG_RISCV_IDX_MASK,
        })
    }
}

pub(super) fn get_one_reg(vcpu: &Vcpu<CurrentArch>, id: u64) -> VfsResult<u64> {
    let reg = RiscvReg::decode(id)?;
    match reg.ty {
        KVM_REG_RISCV_CONFIG => match reg.idx {
            CONFIG_ISA => Ok(isa_bitmap()),
            // No cache-block extension is reported, so the block size is moot.
            CONFIG_ZICBOM_BLOCK_SIZE | CONFIG_ZICBOZ_BLOCK_SIZE | CONFIG_ZICBOP_BLOCK_SIZE => Ok(0),
            CONFIG_MVENDORID | CONFIG_MARCHID | CONFIG_MIMPID => Ok(0),
            CONFIG_SATP_MODE => Ok(SATP_MODE_SV39),
            _ => Err(VfsError::InvalidInput),
        },
        KVM_REG_RISCV_CORE => match reg.idx {
            CORE_PC => Ok(vcpu.arch.pc),
            1..=31 => Ok(vcpu.arch.gprs[reg.idx as usize]),
            CORE_MODE => Ok(KVM_RISCV_MODE_S),
            _ => Err(VfsError::InvalidInput),
        },
        KVM_REG_RISCV_CSR if reg.subtype == KVM_REG_RISCV_SINGLE => match reg.idx {
            CSR_SSTATUS => Ok(vcpu.arch.vsstatus),
            CSR_SIE => Ok(vcpu.arch.vsie),
            CSR_STVEC => Ok(vcpu.arch.vstvec),
            CSR_SSCRATCH => Ok(vcpu.arch.vsscratch),
            // `pc` is the authoritative resume PC; the raw `vsepc` snapshot is
            // overwritten from it by the guest-entry assembly.
            CSR_SEPC => Ok(vcpu.arch.pc),
            CSR_SCAUSE => Ok(vcpu.arch.scause_save),
            CSR_STVAL => Ok(vcpu.arch.stval_save),
            CSR_SIP | CSR_SCOUNTEREN | CSR_SENVCFG => Ok(0),
            CSR_SATP => Ok(vcpu.arch.vsatp),
            _ => Err(VfsError::InvalidInput),
        },
        KVM_REG_RISCV_TIMER => match reg.idx {
            // Must be the frequency of the counter the guest actually reads,
            // because it becomes the guest FDT's `timebase-frequency`.
            TIMER_FREQUENCY => Ok(khal::time::frequency().as_hz()),
            TIMER_TIME => Ok(rv::read_time()),
            TIMER_COMPARE => Ok(vcpu.arch.timer_deadline),
            TIMER_STATE => Ok(if vcpu.arch.timer_deadline != 0 {
                KVM_RISCV_TIMER_STATE_ON
            } else {
                KVM_RISCV_TIMER_STATE_OFF
            }),
            _ => Err(VfsError::InvalidInput),
        },
        // Never fail an in-range extension query: kvmtool's FDT generator
        // reuses one output variable across its extension loop, so an error
        // would leave the previous extension's answer in place and advertise an
        // unsupported extension to the guest.
        KVM_REG_RISCV_ISA_EXT => Ok(u64::from(
            reg.idx < ISA_EXT_MAX && SUPPORTED_ISA_EXTS.contains(&reg.idx),
        )),
        KVM_REG_RISCV_SBI_EXT => Ok(u64::from(
            reg.idx < SBI_EXT_MAX && SUPPORTED_SBI_EXTS.contains(&reg.idx),
        )),
        _ => Err(VfsError::InvalidInput),
    }
}

pub(super) fn set_one_reg(vcpu: &mut Vcpu<CurrentArch>, id: u64, value: u64) -> VfsResult<usize> {
    let reg = RiscvReg::decode(id)?;
    match reg.ty {
        KVM_REG_RISCV_CONFIG => match reg.idx {
            // lkvm reads `isa`, edits it and writes the result straight back,
            // so this must succeed even though the value is not retained.
            CONFIG_ISA | CONFIG_MVENDORID | CONFIG_MARCHID | CONFIG_MIMPID => Ok(0),
            _ => Err(VfsError::InvalidInput),
        },
        KVM_REG_RISCV_CORE => match reg.idx {
            CORE_PC => {
                vcpu.arch.pc = value;
                Ok(0)
            }
            1..=31 => {
                vcpu.arch.gprs[reg.idx as usize] = value;
                Ok(0)
            }
            CORE_MODE if value == KVM_RISCV_MODE_U || value == KVM_RISCV_MODE_S => Ok(0),
            _ => Err(VfsError::InvalidInput),
        },
        KVM_REG_RISCV_CSR if reg.subtype == KVM_REG_RISCV_SINGLE => match reg.idx {
            CSR_SSTATUS => {
                vcpu.arch.vsstatus = value;
                Ok(0)
            }
            CSR_SIE => {
                vcpu.arch.vsie = value;
                Ok(0)
            }
            CSR_STVEC => {
                vcpu.arch.vstvec = value;
                Ok(0)
            }
            CSR_SSCRATCH => {
                vcpu.arch.vsscratch = value;
                Ok(0)
            }
            CSR_SEPC => {
                vcpu.arch.pc = value;
                Ok(0)
            }
            CSR_SATP => {
                vcpu.arch.vsatp = value;
                Ok(0)
            }
            _ => Err(VfsError::InvalidInput),
        },
        KVM_REG_RISCV_TIMER => match reg.idx {
            TIMER_COMPARE => {
                vcpu.arch.timer_deadline = value;
                Ok(0)
            }
            _ => Err(VfsError::InvalidInput),
        },
        // Extension enable/disable banks. lkvm only writes these for extensions
        // it was told about, or after reading them back, and it treats a
        // failure on the enable path as fatal; accept and ignore.
        KVM_REG_RISCV_ISA_EXT | KVM_REG_RISCV_SBI_EXT => Ok(0),
        _ => Err(VfsError::InvalidInput),
    }
}

/// Bitmap of [`SUPPORTED_ISA_EXTS`], as `config.isa` reports it.
fn isa_bitmap() -> u64 {
    SUPPORTED_ISA_EXTS
        .iter()
        .fold(0u64, |bitmap, &ext| bitmap | (1u64 << ext))
}

pub(super) fn complete_run_exit(
    file: &KvmVcpuFile,
    vcpu: &mut Vcpu<CurrentArch>,
) -> VfsResult<bool> {
    let cause = vcpu.arch.scause_save;

    // Bit 63 marks an interrupt. The host IRQ has already been serviced by the
    // time `enter_guest` returned, so re-enter the guest.
    if cause >> 63 != 0 {
        file.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);
        return Ok(false);
    }

    match cause {
        CAUSE_VIRTUAL_INSTRUCTION => complete_wfi_exit(file, vcpu),
        CAUSE_VS_ECALL => match rv::handle_sbi_ecall(vcpu) {
            rv::SbiOutcome::Handled => {
                file.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);
                Ok(false)
            }
            rv::SbiOutcome::Shutdown => {
                file.report_shutdown();
                Ok(true)
            }
        },
        CAUSE_INST_GUEST_PAGE_FAULT
        | CAUSE_LOAD_GUEST_PAGE_FAULT
        | CAUSE_STORE_GUEST_PAGE_FAULT => complete_mmio_exit(file, vcpu),
        cause => {
            log::warn!("[kvm] unsupported guest exit scause={:#x}", cause);
            Err(VfsError::Io)
        }
    }
}

fn complete_wfi_exit(file: &KvmVcpuFile, vcpu: &mut Vcpu<CurrentArch>) -> VfsResult<bool> {
    // hstatus.VTW trapped the WFI before it executed, so step past it.
    vcpu.arch.pc += 4;
    file.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);

    // Deliberately bounded, unlike kvmm's `handle_wfi`: that one sleeps until
    // the SBI deadline and, with no timer armed, until the end of time, which
    // would park KVM_RUN where lkvm cannot interrupt it. Re-entering every
    // millisecond also re-runs the entry hooks, which is what turns a due timer
    // deadline into `hvip.VSTIP`.
    let deadline = khal::time::monotonic_time() + ktime_types::TimeSpan::from_millis(1);
    vcpu.vm
        .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::WfiSleeping);
    ktask::interruptible_sleep_until(deadline);
    vcpu.vm
        .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::HostHandlingExit);
    Ok(false)
}

fn complete_mmio_exit(file: &KvmVcpuFile, vcpu: &mut Vcpu<CurrentArch>) -> VfsResult<bool> {
    let access = match rv::handle_mmio_fault(vcpu) {
        rv::MmioFaultOutcome::Handled(access) => {
            let mmio = MmioExit {
                gpa: access.gpa,
                is_write: access.is_write,
                size: access.size,
                value: access.value,
                reg: access.reg,
            };
            file.log_kernel_mmio(&mmio);
            file.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);
            return Ok(false);
        }
        rv::MmioFaultOutcome::Userspace(access) => access,
        rv::MmioFaultOutcome::Undecodable => return Err(VfsError::Io),
    };

    file.report_userspace_mmio(MmioExit {
        gpa: access.gpa,
        is_write: access.is_write,
        size: access.size,
        value: access.value,
        reg: access.reg,
    });
    // kvmm left the PC on the faulting instruction so lkvm could see it; step
    // past it now, by the instruction's own length (2 when compressed).
    vcpu.arch.pc += access.inst_len;
    Ok(true)
}
