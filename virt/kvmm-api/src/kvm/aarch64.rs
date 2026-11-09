// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! aarch64 half of the `/dev/kvm` compatibility device.
//!
//! Owns everything that depends on the guest's KVM register ABI, exception
//! encoding and device model. The shared fd plumbing in the parent module calls
//! into the same set of names as [`super::riscv64`] does, so the parent stays
//! architecture-independent.
//!
//! lkvm's aarch64 build drives a GICv2 guest: it asks for the in-kernel irqchip
//! with `KVM_CREATE_IRQCHIP`, then hands over both bank addresses, which is what
//! [`prepare_vm`] waits for before wiring the vGIC.

use alloc::sync::Arc;
use core::sync::atomic::Ordering;

use kvfs::{VfsError, VfsResult};
use kvmm::{Vcpu, Vm, arch::CurrentArch, mm::GuestPerm, vcpu_state::VcpuRunState};
use posix_types::{UserConstPtr, UserPtr, UserRead, UserWrite};

use super::{DtbPatch, KVM_RUN_EXIT_REASON_OFF, KvmVcpuFile, KvmVmFile, MmioExit};

// VM-fd ioctls. `KVM_ARM_SET_DEVICE_ADDR` shares its value with
// `KVM_GET_ONE_REG`; Linux disambiguates the two by fd level, and so do we.
const KVM_IRQ_LINE: u32 = 0x4008_ae61;
const KVM_CREATE_IRQCHIP: u32 = 0xae60;
const KVM_ARM_SET_DEVICE_ADDR: u32 = 0x4010_aeab;
const KVM_ARM_PREFERRED_TARGET: u32 = 0x8020_aeaf;
// vCPU-fd ioctl.
const KVM_ARM_VCPU_INIT: u32 = 0x4020_aeae;

const KVM_CAP_IRQCHIP: usize = 0;
const KVM_CAP_ARM_PSCI: usize = 87;
const KVM_CAP_ARM_SET_DEVICE_ADDR: usize = 88;
const KVM_CAP_DEVICE_CTRL: usize = 89;
const KVM_CAP_ARM_PSCI_0_2: usize = 102;

const KVM_ARM_DEVICE_TYPE_MASK: u64 = 0xffff;
const KVM_ARM_DEVICE_ID_SHIFT: u64 = 16;
const KVM_ARM_DEVICE_VGIC_V2: u64 = 0;
const KVM_VGIC_V2_ADDR_TYPE_DIST: u64 = 0;
const KVM_VGIC_V2_ADDR_TYPE_CPU: u64 = 1;
const KVM_ARM_IRQ_TYPE_SHIFT: u32 = 24;
const KVM_ARM_IRQ_TYPE_SPI: u32 = 1;
const KVM_ARM_IRQ_NUM_MASK: u32 = 0xffff;

const KVM_REG_ARM64: u64 = 0x6000_0000_0000_0000;
const KVM_REG_SIZE_U64: u64 = 0x0030_0000_0000_0000;
const KVM_REG_ARM_CORE: u64 = 0x0010 << 16;
const KVM_REG_ARM64_SYSREG: u64 = 0x0013 << 16;
const KVM_REG_ARM64_SYSREG_MPIDR_EL1: u64 =
    KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM64_SYSREG | (3 << 14) | 5;

// `KVM_ARM_CORE_*` offsets are u32 indices into Linux's `struct kvm_regs`, so
// `x0..x30` occupy the even offsets 0..=60 and `offset / 2` is the GPR index.
const KVM_ARM_CORE_X0_U32_OFF: u64 = 0;
const KVM_ARM_CORE_SP_U32_OFF: u64 = 62;
const KVM_ARM_CORE_PC_U32_OFF: u64 = 64;
const KVM_ARM_CORE_PSTATE_U32_OFF: u64 = 66;
const KVM_ARM_TARGET_GENERIC_V8: u32 = 5;

const EC_WFI_WFE: u32 = 0x1;
const EC_HVC64: u32 = 0x16;
const EC_SMC64: u32 = 0x17;
const EC_DATA_ABORT_LOWER: u32 = 0x24;

const KVMTOOL_NS16550_BASE: u64 = 0x0100_0000;
const KVMTOOL_NS16550_SIZE: u64 = 0x100;

/// Extra kernel command line appended to the guest's own `bootargs`.
///
/// The console address is the ns16550 window punched out below: lkvm emulates
/// that UART in userspace, so without `earlycon` the guest stays silent until
/// its driver probes.
pub(super) const EXTRA_BOOTARGS: &[u8] =
    b" earlycon=uart8250,mmio,0x1000000 console=ttyS0 ignore_loglevel";

#[repr(C)]
#[derive(Clone, Copy)]
struct KvmIrqLevel {
    irq: u32,
    level: u32,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KvmArmDeviceAddr {
    id: u64,
    addr: u64,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct KvmVcpuInit {
    target: u32,
    features: [u32; 7],
}

// SAFETY: mirrors Linux `struct kvm_irq_level`, two initialized `u32` fields.
unsafe impl UserRead for KvmIrqLevel {}
// SAFETY: mirrors Linux `struct kvm_arm_device_addr`, two initialized `u64` fields.
unsafe impl UserRead for KvmArmDeviceAddr {}
// SAFETY: mirrors Linux arm64 `struct kvm_vcpu_init`, initialized `u32` fields.
unsafe impl UserRead for KvmVcpuInit {}
// SAFETY: `KvmVcpuInit` contains only initialized scalar fields.
unsafe impl UserWrite for KvmVcpuInit {}

/// Whether hardware discards a write to guest register `reg`.
///
/// AArch64's hardwired-zero register is `XZR`/`WZR` (31); x0 is an ordinary
/// register. Callers must not model this with "register 0".
pub(super) const fn mmio_reg_is_discarded(reg: usize) -> bool {
    reg == 31
}

pub(super) fn check_extension(cap: usize) -> usize {
    match cap {
        KVM_CAP_IRQCHIP => 1,
        KVM_CAP_ARM_PSCI | KVM_CAP_ARM_PSCI_0_2 => 1,
        KVM_CAP_ARM_SET_DEVICE_ADDR | KVM_CAP_DEVICE_CTRL => 1,
        _ => 0,
    }
}

/// Force the guest's PSCI conduit to SMC so shutdown traps to EL2.
///
/// lkvm normally picks HVC; rewriting the property makes the same call arrive
/// as an SMC and reach [`complete_psci_exit`].
pub(super) const DTB_PATCHES: &[DtbPatch] = &[DtbPatch {
    path: "/psci",
    prop: "method",
    value: b"smc\0",
    label: "PSCI method",
}];

/// The guest's boot device tree is passed in x0 on aarch64.
pub(super) fn guest_dtb_user_addr(vm_file: &KvmVmFile, vcpu: &Vcpu<CurrentArch>) -> Option<usize> {
    vm_file.guest_userspace_addr(vcpu.arch.gprs[0])
}

pub(super) fn log_run_start(vcpu: &Vcpu<CurrentArch>) {
    log::info!(
        "[kvm] KVM_RUN start vcpu{} pc={:#x} sp={:#x}",
        vcpu.vcpu_id,
        vcpu.arch.elr,
        vcpu.arch.sp_el1,
    );
}

/// aarch64 needs nothing here: the vGIC injects from the host IRQ path directly,
/// and its per-vCPU hooks were installed by [`prepare_vm`].
pub(super) fn apply_irq_state(_file: &KvmVcpuFile, _vcpu: &mut Vcpu<CurrentArch>) {}

/// Wire the guest's devices once lkvm has supplied both GICv2 bank addresses.
pub(super) fn prepare_vm(vm: &mut Vm<CurrentArch>, vm_file: &KvmVmFile) -> VfsResult<()> {
    if !vm_file.legacy_irqchip_requested.load(Ordering::Acquire)
        || vm_file.gic_created.load(Ordering::Acquire)
    {
        return Ok(());
    }
    let (Some(gicd_base), Some(gicc_base)) = ({
        let addrs = vm_file.legacy_gic_addrs.lock();
        (addrs.dist, addrs.cpu)
    }) else {
        return Ok(());
    };
    vm_file
        .gic_created
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| VfsError::ResourceBusy)?;
    setup_kvmtool_devices(vm, gicd_base, gicc_base).inspect_err(|_| {
        vm_file.gic_created.store(false, Ordering::Release);
    })?;
    Ok(())
}

fn setup_kvmtool_devices(
    vm: &mut Vm<CurrentArch>,
    gicd_base: u64,
    gicc_base: u64,
) -> VfsResult<()> {
    use kalloc_types::KBox;
    use kvmm::vdev::{
        IrqController, IrqSender, VcpuWaker,
        aarch64::{vgic, vgicd, vtimer},
    };

    const GICH_BASE: u64 = 0x0803_0000;
    const GICV_BASE: u64 = 0x0804_0000;
    const GICD_SIZE: u64 = 0x1_0000;
    const GIC_IF_SIZE: u64 = 0x1_0000;

    // Punch the lkvm-emulated UART and the emulated GICD out of the guest so
    // their accesses trap, and pass the CPU interface straight through to the
    // hardware GICV.
    if !vm.unmap_guest_region(KVMTOOL_NS16550_BASE, KVMTOOL_NS16550_SIZE)
        || !vm.unmap_guest_region(gicd_base, GICD_SIZE)
        || !vm.map_guest_region(gicc_base, GICV_BASE, GIC_IF_SIZE, GuestPerm::DeviceRw)
    {
        return Err(VfsError::NoSuchDevice);
    }

    let gich_va = memspace::iomap_device(
        memaddr::PhysAddr::from(GICH_BASE as usize),
        0x1000,
        "kvmm-api-kvm-gich",
    )
    .map_err(|_| VfsError::NoSuchDevice)?;
    let waker = Arc::downgrade(&(vm.shared().clone() as Arc<dyn VcpuWaker>));
    let vgic = vgic::Vgic::new(vm.shared().nr_vcpus(), gich_va.as_usize(), waker);
    let register_storage =
        KBox::new_nowait([0u8; vgicd::REG_SIZE]).map_err(|_| VfsError::NoMemory)?;
    let vgicd = vgicd::Vgicd::new_at(
        gicd_base,
        vgic.clone(),
        vm.shared().nr_vcpus(),
        register_storage,
    );
    let distributor = KBox::new_nowait(vgicd).map_err(|_| VfsError::NoMemory)?;
    vm.shared().devices().register_mmio(distributor);
    vm.shared()
        .devices()
        .set_irq_controller(vgic.clone() as Arc<dyn IrqController>);
    vm.shared()
        .devices()
        .set_irq_sender(vgic.clone() as Arc<dyn IrqSender>);
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(vtimer::VtimerHookFactory));
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(vgic::VgicHookFactory::new(vgic)));
    log::info!(
        "[kvm] kvmtool devices wired: ns16550@{:#x}, GICD@{:#x}, GICC@{:#x}",
        KVMTOOL_NS16550_BASE,
        gicd_base,
        gicc_base,
    );
    Ok(())
}

pub(super) fn vm_ioctl(vm_file: &Arc<KvmVmFile>, cmd: u32, arg: usize) -> Option<VfsResult<usize>> {
    match cmd {
        KVM_CREATE_IRQCHIP => Some(create_irqchip(vm_file)),
        KVM_ARM_SET_DEVICE_ADDR => Some(set_device_addr(vm_file, arg)),
        KVM_ARM_PREFERRED_TARGET => Some(preferred_target(arg)),
        KVM_IRQ_LINE => Some(irq_line(vm_file, arg)),
        _ => None,
    }
}

pub(super) fn vcpu_ioctl(
    _vcpu_file: &KvmVcpuFile,
    cmd: u32,
    arg: usize,
) -> Option<VfsResult<usize>> {
    match cmd {
        KVM_ARM_VCPU_INIT => Some(arm_vcpu_init(arg)),
        _ => None,
    }
}

fn create_irqchip(vm_file: &KvmVmFile) -> VfsResult<usize> {
    vm_file
        .legacy_irqchip_requested
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .map_err(|_| VfsError::ResourceBusy)?;
    Ok(0)
}

fn set_device_addr(vm_file: &KvmVmFile, arg: usize) -> VfsResult<usize> {
    let addr = UserConstPtr::<KvmArmDeviceAddr>::from(arg).read_vm()?;
    let device = addr.id >> KVM_ARM_DEVICE_ID_SHIFT;
    let addr_type = addr.id & KVM_ARM_DEVICE_TYPE_MASK;
    if device != KVM_ARM_DEVICE_VGIC_V2 {
        return Err(VfsError::InvalidInput);
    }
    match addr_type {
        KVM_VGIC_V2_ADDR_TYPE_DIST => vm_file.legacy_gic_addrs.lock().dist = Some(addr.addr),
        KVM_VGIC_V2_ADDR_TYPE_CPU => vm_file.legacy_gic_addrs.lock().cpu = Some(addr.addr),
        _ => return Err(VfsError::InvalidInput),
    }
    Ok(0)
}

fn preferred_target(arg: usize) -> VfsResult<usize> {
    UserPtr::<KvmVcpuInit>::from(arg).write_vm(KvmVcpuInit {
        target: KVM_ARM_TARGET_GENERIC_V8,
        features: [0; 7],
    })?;
    Ok(0)
}

fn irq_line(vm_file: &KvmVmFile, arg: usize) -> VfsResult<usize> {
    let irq_level = UserConstPtr::<KvmIrqLevel>::from(arg).read_vm()?;
    if irq_level.level != 0
        && let Some(intid) = decode_arm_irq_line(irq_level.irq)
    {
        let vm = vm_file.vm.lock();
        vm.shared().inject_irq(0, intid);
    }
    Ok(0)
}

fn decode_arm_irq_line(irq: u32) -> Option<u32> {
    let irq_type = irq >> KVM_ARM_IRQ_TYPE_SHIFT;
    let irq_num = irq & KVM_ARM_IRQ_NUM_MASK;
    (irq_type == KVM_ARM_IRQ_TYPE_SPI).then_some(irq_num)
}

fn arm_vcpu_init(arg: usize) -> VfsResult<usize> {
    let init = UserConstPtr::<KvmVcpuInit>::from(arg).read_vm()?;
    if init.target != KVM_ARM_TARGET_GENERIC_V8 {
        return Err(VfsError::InvalidInput);
    }
    Ok(0)
}

pub(super) fn get_one_reg(vcpu: &Vcpu<CurrentArch>, id: u64) -> VfsResult<u64> {
    match id {
        // The only readable register; lkvm uses it to map CPU ids.
        KVM_REG_ARM64_SYSREG_MPIDR_EL1 => Ok(vcpu.vcpu_id as u64),
        _ => Err(VfsError::InvalidInput),
    }
}

pub(super) fn set_one_reg(
    vcpu: &mut Vcpu<CurrentArch>,
    reg_id: u64,
    value: u64,
) -> VfsResult<usize> {
    if (reg_id & (KVM_REG_ARM64 | KVM_REG_ARM_CORE)) != (KVM_REG_ARM64 | KVM_REG_ARM_CORE) {
        return Err(VfsError::InvalidInput);
    }
    let offset = reg_id & !(KVM_REG_ARM64 | KVM_REG_SIZE_U64 | KVM_REG_ARM_CORE);
    match offset {
        KVM_ARM_CORE_X0_U32_OFF..=60 if offset.is_multiple_of(2) => {
            vcpu.arch.gprs[(offset / 2) as usize] = value;
        }
        KVM_ARM_CORE_SP_U32_OFF => vcpu.arch.sp_el1 = value,
        KVM_ARM_CORE_PC_U32_OFF => vcpu.arch.elr = value,
        KVM_ARM_CORE_PSTATE_U32_OFF => vcpu.arch.spsr = value,
        _ => return Err(VfsError::InvalidInput),
    }
    Ok(0)
}

pub(super) fn complete_run_exit(
    file: &KvmVcpuFile,
    vcpu: &mut Vcpu<CurrentArch>,
) -> VfsResult<bool> {
    // A non-zero `exit_type` means the exit was IRQ/FIQ/SError rather than a
    // synchronous exception: the host interrupt has already been serviced by the
    // time `enter_guest` returned, so just re-enter the guest.
    if vcpu.arch.exit_type != 0 {
        file.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);
        return Ok(false);
    }

    let esr = vcpu.arch.esr;
    match (esr >> 26) as u32 {
        EC_WFI_WFE => complete_wfx_exit(file, vcpu, esr),
        EC_HVC64 | EC_SMC64 => complete_psci_exit(file, vcpu),
        EC_DATA_ABORT_LOWER => complete_mmio_exit(file, vcpu, esr),
        ec => {
            log::warn!("[kvm] unsupported guest exit EC={:#x} ESR={:#x}", ec, esr);
            Err(VfsError::Io)
        }
    }
}

fn complete_wfx_exit(
    file: &KvmVcpuFile,
    vcpu: &mut Vcpu<CurrentArch>,
    esr: u64,
) -> VfsResult<bool> {
    let step = if (esr >> 25) & 1 == 1 { 4 } else { 2 };
    vcpu.arch.elr += step;
    file.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);
    let deadline = khal::time::monotonic_time() + ktime_types::TimeSpan::from_millis(1);
    vcpu.vm
        .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::WfiSleeping);
    ktask::interruptible_sleep_until(deadline);
    vcpu.vm
        .set_vcpu_run_state(vcpu.vcpu_id, VcpuRunState::HostHandlingExit);
    Ok(false)
}

fn complete_mmio_exit(
    file: &KvmVcpuFile,
    vcpu: &mut Vcpu<CurrentArch>,
    esr: u64,
) -> VfsResult<bool> {
    let ipa = (vcpu.arch.hpfar << 8) | (vcpu.arch.far & 0xfff);
    // Without a valid instruction syndrome we cannot tell a load from a store
    // or which register to write back.
    if (esr >> 24) & 1 == 0 {
        return Err(VfsError::Io);
    }
    let size = 1u32 << ((esr >> 22) & 0x3);
    let srt = ((esr >> 16) & 0x1f) as usize;
    let is_write = (esr >> 6) & 1 != 0;
    let value = if is_write && srt != 31 {
        vcpu.arch.gprs[srt]
    } else {
        0
    };

    let mmio = MmioExit {
        gpa: ipa,
        is_write,
        size: size as u8,
        value,
        reg: srt,
    };

    if let Some(handled) = try_kernel_mmio(vcpu, &mmio) {
        file.log_kernel_mmio(&handled);
        file.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);
        return Ok(false);
    }

    // lkvm never advances the PC itself, so do it here; the read result it
    // returns on the next KVM_RUN is delivered by `complete_pending_mmio_read`.
    file.report_userspace_mmio(mmio);
    vcpu.arch.elr += 4;
    Ok(true)
}

/// Offer an MMIO access to the kernel's virtual devices.
///
/// Returns the access with `value` replaced by the device's answer when one
/// claimed it, having already written the destination register and stepped the
/// PC past the faulting instruction.
fn try_kernel_mmio(vcpu: &mut Vcpu<CurrentArch>, mmio: &MmioExit) -> Option<MmioExit> {
    let handled = vcpu.vm.mmio_bus().lock().handle_for_vcpu(
        mmio.gpa,
        mmio.is_write,
        mmio.size,
        mmio.value,
        vcpu.vcpu_id,
    )?;
    if !mmio.is_write && mmio.reg != 31 {
        vcpu.arch.gprs[mmio.reg] = handled;
    }
    vcpu.arch.elr += 4;
    Some(MmioExit {
        value: handled,
        ..*mmio
    })
}

fn complete_psci_exit(file: &KvmVcpuFile, vcpu: &mut Vcpu<CurrentArch>) -> VfsResult<bool> {
    use kvmm::vdev::aarch64::vpsci::{self, PsciAction};

    log::info!(
        "[kvm] PSCI exit pc={:#x} ec={:#x} fid={:#x}",
        vcpu.arch.elr,
        vcpu.arch.esr >> 26,
        vcpu.arch.gprs[0],
    );
    match vpsci::handle_psci(&mut vcpu.arch.gprs) {
        Some(PsciAction::Continue) => {
            vcpu.arch.elr += 4;
            file.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);
            Ok(false)
        }
        Some(PsciAction::Shutdown) => {
            vcpu.arch.elr += 4;
            file.report_shutdown();
            Ok(true)
        }
        Some(PsciAction::CpuOn { .. }) => {
            // Multi-vCPU bring-up is not supported on this path.
            vcpu.arch.gprs[0] = vpsci::PSCI_RET_INVALID_PARAMS;
            vcpu.arch.elr += 4;
            file.write_run_u32(KVM_RUN_EXIT_REASON_OFF, 0);
            Ok(false)
        }
        None => Err(VfsError::Io),
    }
}
