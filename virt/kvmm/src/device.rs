// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/kvmm` character device — write-command VM control.
//!
//! Commands are written as text lines:
//!
//! ```text
//! boot <path> [@0xBASE]   # load a raw guest binary and start it (1 vCPU)
//! bootlinux [kernel dtb initrd] [@0xBASE]
//! attach [vm]             # route this fd's writes to <vm>'s console (~. detaches)
//! input <vm> <text>       # push text + newline into <vm>'s console RX
//! irq <vm> <n>            # inject virtual IRQ <n> into <vm> vCPU 0 (M2)
//! dump                    # log a snapshot of all VMs (same as /proc/kvmm)
//! ```
//!
//! This is a debug/bring-up control plane. The `boot` path is FreeRTOS-style:
//! a single raw binary, no DTB and no initrd. The guest is loaded at
//! `base + KERNEL_OFF` and entered there.

use alloc::{boxed::Box, sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicI32, AtomicU64, Ordering};

use kvfs::{DeviceFileOps, NodeFlags, VfsError, VfsFile, VfsFileBuilder, VfsInode, VfsResult};

#[cfg(target_arch = "aarch64")]
use crate::vdev::aarch64::{vgic, vgicd};
#[cfg(target_arch = "riscv64")]
use crate::vdev::riscv64;
use crate::{
    arch::{CurrentArch, VmmArch},
    loader,
    mm::GuestMem,
    vm::{Vm, VmConfig},
};

/// Total guest RAM per VM.
#[cfg(target_arch = "riscv64")]
const MEM_SIZE: u64 = 0x0C00_0000;
/// Total guest RAM per VM.
#[cfg(not(target_arch = "riscv64"))]
const MEM_SIZE: u64 = 0x0C00_0000;
/// Guest binary is loaded (and entered) at `mem_base + KERNEL_OFF`.
const KERNEL_OFF: u64 = 0x0080_0000;
/// Linux Image entry offset from RAM base.
#[cfg(target_arch = "riscv64")]
const LINUX_KERNEL_OFF: u64 = 0x0020_0000;
/// Linux Image entry offset from RAM base.
#[cfg(not(target_arch = "riscv64"))]
const LINUX_KERNEL_OFF: u64 = 0x0008_0000;
/// Default DTB load offset for Linux-style boot.
#[cfg(target_arch = "riscv64")]
const LINUX_DTB_OFF: u64 = 0x0400_0000;
/// Default DTB load offset for Linux-style boot.
#[cfg(not(target_arch = "riscv64"))]
const LINUX_DTB_OFF: u64 = 0x0400_0000;
/// Default initrd load offset for Linux-style boot.
#[cfg(target_arch = "riscv64")]
const LINUX_INITRD_OFF: u64 = 0x0800_0000;
/// Default initrd load offset for Linux-style boot.
#[cfg(not(target_arch = "riscv64"))]
const LINUX_INITRD_OFF: u64 = 0x0800_0000;
/// First auto-allocated guest memory base.
#[cfg(target_arch = "riscv64")]
const GUEST_MEM_BASE: u64 = 0xC000_0000;
/// First auto-allocated guest memory base.
#[cfg(not(target_arch = "riscv64"))]
const GUEST_MEM_BASE: u64 = 0x7000_0000;
/// Stride between auto-allocated VM bases (256 MiB).
const GUEST_MEM_SLOT: u64 = 0x1000_0000;
/// Raw FreeRTOS bring-up guest vCPU count.
const RAW_BOOT_VCPUS: usize = 4;

struct KvmmVmState {
    vm: Vm<CurrentArch>,
    /// Kept alive so the vCPU threads are not dropped.
    _vcpu_threads: Vec<ktask::KtaskRef>,
}

/// `/dev/kvmm` device state: the set of live VMs and console routing.
pub struct KvmmDevice {
    vms: ksync::Mutex<Vec<Option<KvmmVmState>>>,
    /// VM whose console this fd is attached to, or -1.
    attached_vm: AtomicI32,
    /// Monotonic counter for auto-allocating disjoint guest memory bases.
    next_mem_slot: AtomicU64,
}

impl Default for KvmmDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl KvmmDevice {
    pub fn new() -> Self {
        Self {
            vms: ksync::Mutex::new(Vec::new()),
            attached_vm: AtomicI32::new(-1),
            next_mem_slot: AtomicU64::new(0),
        }
    }

    /// Allocate the next disjoint guest memory base for auto-slotted boots.
    fn alloc_mem_base(&self) -> u64 {
        let slot = self.next_mem_slot.fetch_add(1, Ordering::Relaxed);
        GUEST_MEM_BASE + slot * GUEST_MEM_SLOT
    }

    /// Push one byte into `vm_id`'s console RX FIFO.
    fn push_to_console(&self, vm_id: i32, byte: u8) {
        if vm_id < 0 {
            return;
        }
        let vms = self.vms.lock();
        if let Some(Some(state)) = vms.get(vm_id as usize) {
            state.vm.shared().devices().push_console(byte);
        }
    }

    /// Inject a virtual IRQ into `vm_id` vCPU 0 (wired to the vGIC in M2).
    fn inject_irq(&self, vm_id: i32, irq: u32) {
        if vm_id < 0 {
            return;
        }
        let vms = self.vms.lock();
        if let Some(Some(state)) = vms.get(vm_id as usize) {
            state.vm.shared().inject_irq(0, irq);
        }
    }

    /// Load a raw guest binary and start it on a fresh VM.
    fn handle_boot_cmd(&self, path: &str, base: u64) -> VfsResult<usize> {
        let vm_id = self.vms.lock().len() as u32;
        let kernel_load = base + KERNEL_OFF;

        let cfg = VmConfig::new(base, MEM_SIZE, RAW_BOOT_VCPUS);
        #[cfg_attr(not(target_arch = "aarch64"), allow(unused_mut))]
        let mut vm: Vm<CurrentArch> = Vm::new(cfg).ok_or(VfsError::NoMemory)?;

        // Load the binary into guest RAM.
        {
            let gm = vm.shared().guest_mem().ok_or(VfsError::NoSuchDevice)?;
            let n = loader::load_image_to_guest(gm, path, kernel_load).map_err(|e| {
                log::error!("[kvmm] boot: load {:?}: {:?}", path, e);
                VfsError::InvalidInput
            })?;
            log::info!(
                "[kvmm] boot: vm={} loaded {} bytes of {:?} @ {:#x}",
                vm_id,
                n,
                path,
                kernel_load,
            );
        }

        // Register the console UART.
        #[cfg(target_arch = "aarch64")]
        let (uart, rx) = vdev_vpl011::Vpl011::new(vm_id);
        #[cfg(target_arch = "aarch64")]
        vm.shared().devices().set_console_rx(rx);
        #[cfg(target_arch = "aarch64")]
        vm.shared().devices().register_mmio(Box::new(uart));
        #[cfg(target_arch = "aarch64")]
        unmap_mmio_ranges(&mut vm)?;
        #[cfg(target_arch = "riscv64")]
        {
            let (uart, rx) = vdev_uart16550::Uart16550::new(vm_id);
            vm.shared().devices().set_console_rx(rx);
            vm.shared()
                .devices()
                .register_mmio(Box::new(vdev_uart16550::Uart16550Mmio::new(uart)));
        }

        // AArch64: emulate the GIC distributor, pass GICC→GICV, create the vGIC.
        // Must run before `create_vcpu` clones the shared Arc (guest_mem_mut).
        #[cfg(target_arch = "aarch64")]
        setup_gic(&mut vm)?;
        #[cfg(target_arch = "riscv64")]
        setup_riscv64_devices(&vm);

        if let Some(irq_sender) = vm.shared().devices().irq_sender() {
            let dma = crate::vdev::dma::make_guest_dma(Arc::clone(vm.shared()));
            vm.shared()
                .devices()
                .register_mmio(Box::new(vdev_virtio_net::VirtioNet::new(
                    vm_id, irq_sender, dma,
                )));
        } else {
            log::warn!("[kvmm] boot: virtio-net disabled, no IRQ controller");
        }

        if !CurrentArch::percpu_hw_init() {
            log::error!("[kvmm] boot: per-CPU HW init failed");
            return Err(VfsError::NoSuchDevice);
        }

        #[cfg_attr(not(target_arch = "aarch64"), allow(unused_mut))]
        let mut vcpu = vm.create_vcpu(0);

        // Raw-binary boot uses guest physical entry addresses, so set the arch
        // PC directly rather than via `init_vcpu` (which translates host VAs).
        #[cfg(not(any(target_arch = "aarch64", target_arch = "riscv64")))]
        {
            let _ = (vcpu, vm, kernel_load, vm_id);
            Err(VfsError::NoSuchDevice)
        }
        #[cfg(target_arch = "riscv64")]
        {
            vcpu.arch.pc = kernel_load;
            vcpu.arch.gprs[2] = 0;

            vm.shared().try_mark_cpu_on(0);
            vm.register();
            let task = crate::vcpu::spawn_vcpu_thread::<CurrentArch>(vcpu);
            let mut vms = self.vms.lock();
            vms.push(Some(KvmmVmState {
                vm,
                _vcpu_threads: alloc::vec![task],
            }));
            log::info!("[kvmm] boot: rv64 vm={} started", vm_id);
            Ok(0)
        }
        #[cfg(target_arch = "aarch64")]
        {
            vcpu.arch.elr = kernel_load;
            vcpu.arch.sp_el1 = 0;
            vcpu.arch.spsr = 0x5 | (0xF << 6); // EL1h, DAIF masked
            vcpu.arch.gprs[0] = 0; // no DTB

            vm.shared().try_mark_cpu_on(0);
            vm.register();
            let task = crate::vcpu::spawn_vcpu_thread::<CurrentArch>(vcpu);
            let mut vms = self.vms.lock();
            vms.push(Some(KvmmVmState {
                vm,
                _vcpu_threads: alloc::vec![task],
            }));
            log::info!("[kvmm] boot: vm={} started", vm_id);
            Ok(0)
        }
    }

    /// Load an arm64 Linux Image, DTB, and optional initrd, then start vCPU0.
    fn handle_boot_linux_cmd(
        &self,
        kernel_path: &str,
        dtb_path: &str,
        initrd_path: Option<&str>,
        base: u64,
    ) -> VfsResult<usize> {
        let vm_id = self.vms.lock().len() as u32;
        let kernel_load = base + LINUX_KERNEL_OFF;
        let dtb_load = base + LINUX_DTB_OFF;
        let initrd_load = base + LINUX_INITRD_OFF;
        let nr_vcpus = loader::peek_dtb_cpu_count(dtb_path)
            .unwrap_or_else(|err| {
                log::warn!(
                    "[kvmm] bootlinux: peek {:?}: {:?}, defaulting to 1 vCPU",
                    dtb_path,
                    err,
                );
                1
            })
            .max(1);

        let cfg = VmConfig::new(base, MEM_SIZE, nr_vcpus);
        #[cfg_attr(not(target_arch = "aarch64"), allow(unused_mut))]
        let mut vm: Vm<CurrentArch> = Vm::new(cfg).ok_or(VfsError::NoMemory)?;

        {
            let gm = vm.shared().guest_mem().ok_or(VfsError::NoSuchDevice)?;
            let kernel_size =
                loader::load_image_to_guest(gm, kernel_path, kernel_load).map_err(|err| {
                    log::error!("[kvmm] bootlinux: load kernel {:?}: {:?}", kernel_path, err);
                    VfsError::InvalidInput
                })?;
            let dtb_size = loader::load_image_to_guest(gm, dtb_path, dtb_load).map_err(|err| {
                log::error!("[kvmm] bootlinux: load dtb {:?}: {:?}", dtb_path, err);
                VfsError::InvalidInput
            })?;
            loader::patch_dtb_memory(gm, dtb_load, dtb_size, base, MEM_SIZE).map_err(|err| {
                log::error!("[kvmm] bootlinux: patch memory: {:?}", err);
                VfsError::InvalidInput
            })?;
            loader::nop_dtb_nodes(
                gm,
                dtb_load,
                dtb_size,
                &["gpio-keys", "pl061@9030000", "v2m@8020000"],
            )
            .map_err(|err| {
                log::error!("[kvmm] bootlinux: patch unsupported nodes: {:?}", err);
                VfsError::InvalidInput
            })?;

            if let Some(path) = initrd_path {
                let initrd_size =
                    loader::load_image_to_guest(gm, path, initrd_load).map_err(|err| {
                        log::error!("[kvmm] bootlinux: load initrd {:?}: {:?}", path, err);
                        VfsError::InvalidInput
                    })?;
                loader::patch_dtb_initrd(
                    gm,
                    dtb_load,
                    dtb_size,
                    initrd_load,
                    initrd_load + initrd_size as u64,
                )
                .map_err(|err| {
                    log::error!("[kvmm] bootlinux: patch initrd: {:?}", err);
                    VfsError::InvalidInput
                })?;
                log::info!(
                    "[kvmm] bootlinux: initrd {} bytes @ {:#x}",
                    initrd_size,
                    initrd_load,
                );
            }

            log::info!(
                "[kvmm] bootlinux: kernel {} bytes @ {:#x}, dtb {} bytes @ {:#x}",
                kernel_size,
                kernel_load,
                dtb_size,
                dtb_load,
            );
        }

        #[cfg(target_arch = "aarch64")]
        let (uart, rx) = vdev_vpl011::Vpl011::new(vm_id);
        #[cfg(target_arch = "aarch64")]
        vm.shared().devices().set_console_rx(rx);
        #[cfg(target_arch = "aarch64")]
        vm.shared().devices().register_mmio(Box::new(uart));
        #[cfg(target_arch = "aarch64")]
        unmap_mmio_ranges(&mut vm)?;
        #[cfg(target_arch = "riscv64")]
        {
            let (uart, rx) = vdev_uart16550::Uart16550::new(vm_id);
            vm.shared().devices().set_console_rx(rx);
            vm.shared()
                .devices()
                .register_mmio(Box::new(vdev_uart16550::Uart16550Mmio::new(uart)));
        }
        #[cfg(target_arch = "riscv64")]
        unmap_riscv64_mmio_ranges(&mut vm)?;

        #[cfg(target_arch = "aarch64")]
        setup_gic(&mut vm)?;
        #[cfg(target_arch = "riscv64")]
        setup_riscv64_devices(&vm);

        if let Some(irq_sender) = vm.shared().devices().irq_sender() {
            let dma = crate::vdev::dma::make_guest_dma(Arc::clone(vm.shared()));
            vm.shared()
                .devices()
                .register_mmio(Box::new(vdev_virtio_net::VirtioNet::new(
                    vm_id, irq_sender, dma,
                )));
        }

        if !CurrentArch::percpu_hw_init() {
            log::error!("[kvmm] bootlinux: per-CPU HW init failed");
            return Err(VfsError::NoSuchDevice);
        }

        #[cfg_attr(not(target_arch = "aarch64"), allow(unused_mut))]
        let mut vcpu = vm.create_vcpu(0);
        #[cfg(not(any(target_arch = "aarch64", target_arch = "riscv64")))]
        {
            let _ = (vcpu, vm, kernel_load, dtb_load, vm_id);
            Err(VfsError::NoSuchDevice)
        }
        #[cfg(target_arch = "riscv64")]
        {
            vcpu.arch.pc = kernel_load;
            vcpu.arch.gprs[10] = 0; // a0: boot hartid
            vcpu.arch.gprs[11] = dtb_load; // a1: DTB physical address

            vm.shared().try_mark_cpu_on(0);
            vm.register();
            let task = crate::vcpu::spawn_vcpu_thread::<CurrentArch>(vcpu);
            let mut vms = self.vms.lock();
            vms.push(Some(KvmmVmState {
                vm,
                _vcpu_threads: alloc::vec![task],
            }));
            log::info!(
                "[kvmm] bootlinux: rv64 vm={} started vcpus={} entry={:#x} dtb={:#x}",
                vm_id,
                nr_vcpus,
                kernel_load,
                dtb_load,
            );
            Ok(0)
        }
        #[cfg(target_arch = "aarch64")]
        {
            vcpu.arch.elr = kernel_load;
            vcpu.arch.sp_el1 = 0;
            vcpu.arch.spsr = 0x5 | (0xF << 6); // EL1h, DAIF masked
            vcpu.arch.gprs[0] = dtb_load;

            vm.shared().try_mark_cpu_on(0);
            vm.register();
            let task = crate::vcpu::spawn_vcpu_thread::<CurrentArch>(vcpu);
            let mut vms = self.vms.lock();
            vms.push(Some(KvmmVmState {
                vm,
                _vcpu_threads: alloc::vec![task],
            }));
            log::info!(
                "[kvmm] bootlinux: vm={} started vcpus={} entry={:#x} dtb={:#x}",
                vm_id,
                nr_vcpus,
                kernel_load,
                dtb_load,
            );
            Ok(0)
        }
    }
}

#[cfg(target_arch = "aarch64")]
fn unmap_mmio_ranges(vm: &mut Vm<CurrentArch>) -> VfsResult<()> {
    let ranges = vm.shared().devices().mmio_ranges();
    let Some(gm) = vm.guest_mem_mut() else {
        log::error!("[kvmm] guest_mem_mut unavailable for MMIO unmap");
        return Err(VfsError::NoSuchDevice);
    };

    for (name, base, size) in ranges {
        if !gm.unmap_range(base, size) {
            log::error!(
                "[kvmm] failed to unmap MMIO range {} @ {:#x}+{:#x}",
                name,
                base,
                size,
            );
            return Err(VfsError::NoSuchDevice);
        }
        log::info!(
            "[kvmm] unmapped MMIO range {} @ {:#x}+{:#x}",
            name,
            base,
            size
        );
    }
    Ok(())
}

#[cfg(target_arch = "riscv64")]
fn unmap_riscv64_mmio_ranges(vm: &mut Vm<CurrentArch>) -> VfsResult<()> {
    let ranges = [
        (
            "riscv-uart",
            vdev_uart16550::UART_BASE,
            vdev_uart16550::UART_SIZE,
        ),
        (
            "riscv-vplic",
            riscv64::irq::VPLIC_BASE,
            riscv64::irq::VPLIC_SIZE,
        ),
        (
            "virtio-net",
            vdev_virtio_net::VIRTIO_NET_BASE,
            vdev_virtio_net::VIRTIO_NET_SIZE,
        ),
    ];

    let Some(gm) = vm.guest_mem_mut() else {
        log::error!("[kvmm] guest_mem_mut unavailable for RISC-V MMIO unmap");
        return Err(VfsError::NoSuchDevice);
    };

    for (name, base, size) in ranges {
        if !gm.unmap_range(base, size) {
            log::error!(
                "[kvmm] failed to unmap RISC-V MMIO range {} @ {:#x}+{:#x}",
                name,
                base,
                size,
            );
            return Err(VfsError::NoSuchDevice);
        }
        log::info!(
            "[kvmm] unmapped RISC-V MMIO range {} @ {:#x}+{:#x}",
            name,
            base,
            size
        );
    }
    Ok(())
}

/// Wire up the minimal RISC-V interrupt and timer validation devices.
#[cfg(target_arch = "riscv64")]
fn setup_riscv64_devices(vm: &Vm<CurrentArch>) {
    let waker = Arc::downgrade(&(vm.shared().clone() as Arc<dyn crate::vdev::VcpuWaker>));
    let irq = riscv64::irq::RiscvIrq::new(vm.shared().nr_vcpus(), waker);
    vm.shared()
        .devices()
        .register_mmio(Box::new(riscv64::irq::RiscvPlicMmio::new(irq.clone())));
    vm.shared()
        .devices()
        .set_irq_controller(irq.clone() as Arc<dyn crate::vdev::IrqController>);
    vm.shared()
        .devices()
        .set_irq_sender(irq.clone() as Arc<dyn crate::vdev::IrqSender>);
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(riscv64::irq::RiscvIrqHookFactory::new(irq)));
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(riscv64::timer::RiscvTimerHookFactory));
    log::info!("[kvmm] RISC-V IRQ/timer validation devices wired");
}

/// Wire up the AArch64 GIC for a VM: emulate the distributor (GICD), pass the
/// CPU interface (GICC) through to the hardware virtual interface (GICV), and
/// create the vGIC (GICH list-register injector).
///
/// The real GICD/GICC/GICH are never mapped into the guest — GICD traps to
/// [`vgicd`], GICC is redirected to the per-VM GICV, and GICH stays host-only.
#[cfg(target_arch = "aarch64")]
fn setup_gic(vm: &mut Vm<CurrentArch>) -> VfsResult<()> {
    use crate::mm::{GuestMem, GuestPerm};

    const GICC_BASE: u64 = 0x0801_0000;
    const GICH_BASE: u64 = 0x0803_0000;
    const GICV_BASE: u64 = 0x0804_0000;
    const GIC_IF_SIZE: u64 = 0x1_0000;

    // GICC (guest) → GICV (hardware). Must run while the shared Arc is unique
    // (before create_vcpu clones it), since guest_mem_mut needs Arc::get_mut.
    match vm.guest_mem_mut() {
        Some(gm) => {
            if !gm.map_region(GICC_BASE, GICV_BASE, GIC_IF_SIZE, GuestPerm::DeviceRW) {
                log::error!("[kvmm] GICC→GICV map_region failed");
                return Err(VfsError::NoSuchDevice);
            }
        }
        None => {
            log::error!("[kvmm] guest_mem_mut unavailable for GIC map");
            return Err(VfsError::NoSuchDevice);
        }
    }

    // Map GICH (host-only) for the vGIC to program list registers.
    let gich_va = memspace::iomap_device(
        memaddr::PhysAddr::from(GICH_BASE as usize),
        0x1000,
        "kvmm-gich",
    )
    .map_err(|e| {
        log::error!("[kvmm] GICH iomap failed: {:?}", e);
        VfsError::NoSuchDevice
    })?;

    let waker = Arc::downgrade(&(vm.shared().clone() as Arc<dyn crate::vdev::VcpuWaker>));
    let vgic = vgic::Vgic::new(vm.shared().nr_vcpus(), gich_va.as_usize(), waker);
    let vgicd = vgicd::Vgicd::new(vgic.clone(), vm.shared().nr_vcpus());
    vm.shared().devices().register_mmio(Box::new(vgicd));
    vm.shared()
        .devices()
        .set_irq_controller(vgic.clone() as Arc<dyn crate::vdev::IrqController>);
    vm.shared()
        .devices()
        .set_irq_sender(vgic.clone() as Arc<dyn crate::vdev::IrqSender>);
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(crate::vdev::aarch64::vtimer::VtimerHookFactory));
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(vgic::VgicHookFactory::new(vgic)));
    log::info!(
        "[kvmm] GIC wired: GICD emulated, GICC→GICV, GICH@{:#x}",
        gich_va.as_usize()
    );
    Ok(())
}

impl DeviceFileOps for KvmmDevice {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        file.stream_open();
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, _buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(0)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        // Attached mode: forward raw bytes to the attached VM's UART RX.
        let attached = self.attached_vm.load(Ordering::Relaxed);
        if attached >= 0 {
            let cmd = core::str::from_utf8(buf).unwrap_or("");
            if cmd.trim() == "~." {
                self.attached_vm.store(-1, Ordering::Release);
                log::info!("[kvmm] console detached (was vm {})", attached);
                return Ok(buf.len());
            }
            for &b in buf {
                self.push_to_console(attached, b);
            }
            return Ok(buf.len());
        }

        let cmd = core::str::from_utf8(buf)
            .map_err(|_| VfsError::InvalidInput)?
            .trim();
        if cmd.is_empty() {
            return Ok(buf.len());
        }

        if cmd == "attach" {
            self.attached_vm.store(0, Ordering::Release);
            log::info!("[kvmm] console attached to vm 0 (write ~. to detach)");
            Ok(buf.len())
        } else if let Some(rest) = cmd.strip_prefix("attach ") {
            let vm_id = rest
                .trim()
                .parse::<i32>()
                .map_err(|_| VfsError::InvalidInput)?;
            self.attached_vm.store(vm_id, Ordering::Release);
            log::info!(
                "[kvmm] console attached to vm {} (write ~. to detach)",
                vm_id
            );
            Ok(buf.len())
        } else if let Some(rest) = cmd.strip_prefix("bootlinux") {
            // bootlinux [kernel dtb initrd] [@0xBASE]
            let mut kernel = None;
            let mut dtb = None;
            let mut initrd = None;
            let mut mem_base: Option<u64> = None;
            for tok in rest.split_whitespace() {
                if let Some(hex) = tok.strip_prefix('@') {
                    let hex = hex
                        .strip_prefix("0x")
                        .or_else(|| hex.strip_prefix("0X"))
                        .unwrap_or(hex);
                    mem_base =
                        Some(u64::from_str_radix(hex, 16).map_err(|_| VfsError::InvalidInput)?);
                } else if kernel.is_none() {
                    kernel = Some(tok);
                } else if dtb.is_none() {
                    dtb = Some(tok);
                } else if initrd.is_none() {
                    initrd = Some(tok);
                }
            }

            let kernel = kernel.unwrap_or("/guests/linux/linux.bin");
            let dtb = dtb.unwrap_or("/guests/linux/linux.dtb");
            let initrd = initrd.or(Some("/guests/linux/initrd.gz"));
            let base = mem_base.unwrap_or_else(|| self.alloc_mem_base());
            self.handle_boot_linux_cmd(kernel, dtb, initrd, base)?;
            Ok(buf.len())
        } else if let Some(rest) = cmd.strip_prefix("boot ") {
            // boot <path> [@0xBASE]  (FreeRTOS-style: no DTB, no initrd)
            let mut path = None;
            let mut mem_base: Option<u64> = None;
            for tok in rest.split_whitespace() {
                if let Some(hex) = tok.strip_prefix('@') {
                    let hex = hex
                        .strip_prefix("0x")
                        .or_else(|| hex.strip_prefix("0X"))
                        .unwrap_or(hex);
                    mem_base =
                        Some(u64::from_str_radix(hex, 16).map_err(|_| VfsError::InvalidInput)?);
                } else if path.is_none() {
                    path = Some(tok);
                }
            }
            let path = path.ok_or(VfsError::InvalidInput)?;
            let base = mem_base.unwrap_or_else(|| self.alloc_mem_base());
            self.handle_boot_cmd(path, base)?;
            Ok(buf.len())
        } else if let Some(rest) = cmd.strip_prefix("input ") {
            // input <vm_id> <text>
            let mut parts = rest.splitn(2, char::is_whitespace);
            let vm_id = parts
                .next()
                .unwrap_or("")
                .trim()
                .parse::<i32>()
                .map_err(|_| VfsError::InvalidInput)?;
            let text = parts.next().unwrap_or("");
            for &b in text.as_bytes() {
                self.push_to_console(vm_id, b);
            }
            self.push_to_console(vm_id, b'\n');
            Ok(buf.len())
        } else if let Some(rest) = cmd.strip_prefix("irq ") {
            // irq <vm_id> <n>
            let mut parts = rest.split_whitespace();
            let vm_id = parts
                .next()
                .unwrap_or("")
                .parse::<i32>()
                .map_err(|_| VfsError::InvalidInput)?;
            let irq = parts
                .next()
                .unwrap_or("")
                .parse::<u32>()
                .map_err(|_| VfsError::InvalidInput)?;
            self.inject_irq(vm_id, irq);
            log::info!("[kvmm] injected irq {} into vm {}", irq, vm_id);
            Ok(buf.len())
        } else if cmd == "dump" || cmd.starts_with("dump") {
            log::info!("[kvmm] dump:\n{}", crate::vm::dump_vm_info());
            Ok(buf.len())
        } else {
            log::warn!("[kvmm] unknown write command: {:?}", cmd);
            Err(VfsError::InvalidInput)
        }
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}
