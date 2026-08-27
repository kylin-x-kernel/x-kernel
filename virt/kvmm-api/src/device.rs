// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! fd-bound kvmm VM instance device (`/dev/kvmm-vm`).
//!
//! Each open of this device owns exactly one VM whose lifetime is tied to the
//! open file description:
//!
//! * `open` installs a fresh, idle per-fd [`VmInstance`] in the file's private
//!   data.
//! * the **first** `write` carries a `bootlinux [kernel dtb initrd] [@0xBASE]`
//!   command and boots the VM into that instance;
//! * subsequent `write`s are treated as console input and pushed into the
//!   guest UART RX FIFO;
//! * `read` drains the guest→host console TX channel (guest UART output). The
//!   device is non-blocking: `poll` reports readable only when output is
//!   pending, and `read` returns `WouldBlock` on an empty FIFO rather than a
//!   spurious EOF;
//! * `release` (the last close of the open file) requests every vCPU to stop,
//!   joins the vCPU threads, and drops the VM so its guest memory is released.
//!
//! Binding VM lifetime to `release` means a real userspace process — not a
//! transient `echo` — owns the VM, which is the point of this device.
//!
//! ## Scope
//!
//! riscv64 and aarch64 Linux boot are implemented; other architectures return
//! an error from the boot command. This device is the supported Linux boot
//! control plane; the `kvmm` crate only provides the underlying VMM mechanism.

use alloc::sync::Arc;
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
use core::sync::atomic::{AtomicU64, Ordering};

use kpoll::IoEvents;
use kvfs::{DeviceFileOps, NodeFlags, VfsError, VfsFile, VfsFileBuilder, VfsInode, VfsResult};
use kvmm::{Vm, arch::CurrentArch};

use crate::loader;

/// First auto-allocated guest memory base (riscv64).
#[cfg(target_arch = "riscv64")]
const GUEST_MEM_BASE: u64 = 0xC000_0000;
/// First auto-allocated guest memory base (aarch64; matches the guest DTS
/// `memory@70000000`).
#[cfg(target_arch = "aarch64")]
const GUEST_MEM_BASE: u64 = 0x7000_0000;
/// Stride between auto-allocated VM bases (256 MiB).
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
const GUEST_MEM_SLOT: u64 = 0x1000_0000;

/// Monotonic counter handing out disjoint guest memory slots / VM ids.
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
static NEXT_SLOT: AtomicU64 = AtomicU64::new(0);

/// Per-fd VM instance stored in the open file's private data.
///
/// Interior mutability lets `open` install an idle instance and a later
/// `write` fill it in, without threading the VM through the builder.
struct VmInstance {
    state: ksync::Mutex<InstanceState>,
}

enum InstanceState {
    /// No VM booted yet; the next boot command creates one.
    Idle,
    /// A booted VM whose vCPU threads are running.
    Running(Vm<CurrentArch>),
}

impl VmInstance {
    fn new() -> Self {
        Self {
            state: ksync::Mutex::new(InstanceState::Idle),
        }
    }
}

/// fd-bound VM instance control device.
///
/// Stateless itself: all per-VM state lives in the per-fd [`VmInstance`]. See
/// the [module documentation](self) for the lifecycle contract.
pub struct KvmmVmDevice;

impl KvmmVmDevice {
    /// Create the device. One shared instance backs the `/dev/kvmm-vm` node;
    /// per-open state is created in [`DeviceFileOps::open`].
    pub fn new() -> Self {
        Self
    }
}

impl Default for KvmmVmDevice {
    fn default() -> Self {
        Self::new()
    }
}

impl DeviceFileOps for KvmmVmDevice {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        file.stream_open();
        file.set_private_data(Arc::new(VmInstance::new()));
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        let instance = file
            .private_data_get::<VmInstance>()
            .ok_or(VfsError::NoSuchDevice)?;
        let state = instance.state.lock();
        if let InstanceState::Running(vm) = &*state {
            let n = vm.shared().devices().drain_console(buf);
            if n > 0 {
                return Ok(n);
            }
        }
        // No output pending (or not booted). The device is non-blocking; a
        // reader that has not polled gets WouldBlock rather than a spurious EOF
        // (which callers like `kvmm-run` treat as "TX closed"). `poll` gates the
        // normal read path so this branch is not hit in steady state.
        Err(VfsError::WouldBlock)
    }

    fn write(&self, file: &VfsFile, buf: &[u8], _offset: u64) -> VfsResult<usize> {
        let instance = file
            .private_data_get::<VmInstance>()
            .ok_or(VfsError::NoSuchDevice)?;
        let mut state = instance.state.lock();

        // Once booted, writes are console input for the guest UART.
        if let InstanceState::Running(vm) = &*state {
            for &b in buf {
                vm.shared().devices().push_console(b);
            }
            return Ok(buf.len());
        }

        // Idle: the first write is the boot command.
        let cmd = core::str::from_utf8(buf)
            .map_err(|_| VfsError::InvalidInput)?
            .trim();
        let vm = boot(cmd)?;
        *state = InstanceState::Running(vm);
        Ok(buf.len())
    }

    fn release(&self, _inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        if let Some(instance) = file.private_data_get::<VmInstance>() {
            let mut state = instance.state.lock();
            if let InstanceState::Running(vm) = &*state {
                log::info!("[kvmm-api] fd closed: stopping VM and joining vCPU threads");
                vm.stop_and_join();
            }
            // Dropping the VM releases the last `Arc<VmShared>` (vCPU threads
            // have joined), which frees the second-stage page table.
            *state = InstanceState::Idle;
        }
        Ok(())
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        // Always writable (console input is accepted whenever the VM runs).
        // Readable only when the guest has produced output, so a poll-driven
        // reader (e.g. `kvmm-run`) calls `read` exactly when `drain_console`
        // will return bytes, never spinning on an empty FIFO.
        let mut events = IoEvents::OUT;
        if let Some(instance) = file.private_data_get::<VmInstance>()
            && let InstanceState::Running(vm) = &*instance.state.lock()
            && vm.shared().devices().console_has_output()
        {
            events |= IoEvents::IN;
        }
        events
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

/// Parse and dispatch the first-write boot command.
fn boot(cmd: &str) -> VfsResult<Vm<CurrentArch>> {
    let Some(rest) = cmd.strip_prefix("bootlinux") else {
        log::warn!(
            "[kvmm-api] expected 'bootlinux ...' as the first write, got {:?}",
            cmd,
        );
        return Err(VfsError::InvalidInput);
    };
    boot_linux(rest)
}

#[cfg(not(any(target_arch = "riscv64", target_arch = "aarch64")))]
fn boot_linux(_rest: &str) -> VfsResult<Vm<CurrentArch>> {
    log::error!("[kvmm-api] fd-bound VM boot is only implemented on riscv64/aarch64");
    Err(VfsError::NoSuchDevice)
}

/// Parse a `bootlinux` argument tail (`[kernel dtb initrd] [@0xBASE]`), allocate
/// a disjoint guest memory slot, and start vCPU0.
///
/// The parse and slot allocation are architecture-independent; the actual VM
/// construction dispatches to the per-arch `build_and_start_*` helper.
#[cfg(any(target_arch = "riscv64", target_arch = "aarch64"))]
fn boot_linux(rest: &str) -> VfsResult<Vm<CurrentArch>> {
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
            mem_base = Some(u64::from_str_radix(hex, 16).map_err(|_| VfsError::InvalidInput)?);
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

    let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
    let vm_id = slot as u32;
    let base = mem_base.unwrap_or(GUEST_MEM_BASE + slot * GUEST_MEM_SLOT);

    #[cfg(target_arch = "riscv64")]
    {
        build_and_start_rv64(vm_id, base, kernel, dtb, initrd)
    }
    #[cfg(target_arch = "aarch64")]
    {
        build_and_start_aarch64(vm_id, base, kernel, dtb, initrd)
    }
}

#[cfg(target_arch = "riscv64")]
fn build_and_start_rv64(
    vm_id: u32,
    base: u64,
    kernel_path: &str,
    dtb_path: &str,
    initrd_path: Option<&str>,
) -> VfsResult<Vm<CurrentArch>> {
    use alloc::boxed::Box;

    use kvmm::{VmConfig, arch::VmmArch};

    /// Linux Image entry offset from RAM base.
    const LINUX_KERNEL_OFF: u64 = 0x0020_0000;
    /// DTB load offset from RAM base.
    const LINUX_DTB_OFF: u64 = 0x0400_0000;
    /// initrd load offset from RAM base.
    const LINUX_INITRD_OFF: u64 = 0x0800_0000;
    /// Total guest RAM per VM.
    const MEM_SIZE: u64 = 0x0C00_0000;

    let kernel_load = base + LINUX_KERNEL_OFF;
    let dtb_load = base + LINUX_DTB_OFF;
    let initrd_load = base + LINUX_INITRD_OFF;

    let nr_vcpus = loader::peek_dtb_cpu_count(dtb_path)
        .unwrap_or_else(|err| {
            log::warn!(
                "[kvmm-api] bootlinux: peek {:?}: {:?}, defaulting to 1 vCPU",
                dtb_path,
                err,
            );
            1
        })
        .max(1);

    let cfg = VmConfig::new(base, MEM_SIZE, nr_vcpus);
    let mut vm: Vm<CurrentArch> = Vm::new(cfg).ok_or(VfsError::NoMemory)?;

    {
        let gm = vm.shared().guest_mem().ok_or(VfsError::NoSuchDevice)?;
        let kernel_size =
            loader::load_image_to_guest(gm, kernel_path, kernel_load).map_err(|err| {
                log::error!(
                    "[kvmm-api] bootlinux: load kernel {:?}: {:?}",
                    kernel_path,
                    err
                );
                VfsError::InvalidInput
            })?;
        let dtb_size = loader::load_image_to_guest(gm, dtb_path, dtb_load).map_err(|err| {
            log::error!("[kvmm-api] bootlinux: load dtb {:?}: {:?}", dtb_path, err);
            VfsError::InvalidInput
        })?;
        loader::patch_dtb_memory(gm, dtb_load, dtb_size, base, MEM_SIZE).map_err(|err| {
            log::error!("[kvmm-api] bootlinux: patch memory: {:?}", err);
            VfsError::InvalidInput
        })?;
        loader::nop_dtb_nodes(
            gm,
            dtb_load,
            dtb_size,
            &["gpio-keys", "pl061@9030000", "v2m@8020000"],
        )
        .map_err(|err| {
            log::error!("[kvmm-api] bootlinux: patch unsupported nodes: {:?}", err);
            VfsError::InvalidInput
        })?;

        if let Some(path) = initrd_path {
            let initrd_size =
                loader::load_image_to_guest(gm, path, initrd_load).map_err(|err| {
                    log::error!("[kvmm-api] bootlinux: load initrd {:?}: {:?}", path, err);
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
                log::error!("[kvmm-api] bootlinux: patch initrd: {:?}", err);
                VfsError::InvalidInput
            })?;
            log::info!(
                "[kvmm-api] bootlinux: initrd {} bytes @ {:#x}",
                initrd_size,
                initrd_load,
            );
        }

        log::info!(
            "[kvmm-api] bootlinux: kernel {} bytes @ {:#x}, dtb {} bytes @ {:#x}",
            kernel_size,
            kernel_load,
            dtb_size,
            dtb_load,
        );
    }

    // Console UART + its RX/TX channels. Installing the TX channel switches
    // guest output from the host kernel log into the channel drained by this
    // device's `read`.
    let uart = {
        let (uart, rx, tx) = vdev_uart16550::Uart16550::new(vm_id);
        vm.shared().devices().set_console_rx(rx);
        vm.shared().devices().set_console_tx(tx);
        vm.shared()
            .devices()
            .register_mmio(Box::new(vdev_uart16550::Uart16550Mmio::new(uart.clone())));
        uart
    };

    // Unmap emulated MMIO ranges so guest accesses trap into the VMM.
    unmap_rv64_mmio(&mut vm)?;

    // PLIC + timer validation devices, and the UART interrupt line.
    setup_rv64_devices(&vm, &uart);

    if let Some(irq_sender) = vm.shared().devices().irq_sender() {
        let dma = kvmm::vdev::dma::make_guest_dma(Arc::clone(vm.shared()));
        vm.shared()
            .devices()
            .register_mmio(Box::new(vdev_virtio_net::VirtioNet::new(
                vm_id, irq_sender, dma,
            )));
    }

    if !CurrentArch::percpu_hw_init() {
        log::error!("[kvmm-api] bootlinux: per-CPU HW init failed");
        return Err(VfsError::NoSuchDevice);
    }

    let mut vcpu = vm.create_vcpu(0);
    vcpu.arch.pc = kernel_load;
    vcpu.arch.gprs[10] = 0; // a0: boot hartid
    vcpu.arch.gprs[11] = dtb_load; // a1: DTB physical address

    vm.shared().try_mark_cpu_on(0);
    vm.register();
    // The returned handle is dropped: the vCPU task is also published in the
    // VM's `vcpu_tasks` slot, which keeps the thread alive and lets
    // `stop_and_join` reclaim it at teardown.
    let _task = kvmm::spawn_vcpu_thread::<CurrentArch>(vcpu);

    log::info!(
        "[kvmm-api] bootlinux: rv64 vm={} started vcpus={} entry={:#x} dtb={:#x}",
        vm_id,
        nr_vcpus,
        kernel_load,
        dtb_load,
    );
    Ok(vm)
}

#[cfg(target_arch = "riscv64")]
fn unmap_rv64_mmio(vm: &mut Vm<CurrentArch>) -> VfsResult<()> {
    use kvmm::{mm::GuestMem, vdev::riscv64::irq};

    let ranges = [
        (
            "riscv-uart",
            vdev_uart16550::UART_BASE,
            vdev_uart16550::UART_SIZE,
        ),
        ("riscv-vplic", irq::VPLIC_BASE, irq::VPLIC_SIZE),
        (
            "virtio-net",
            vdev_virtio_net::VIRTIO_NET_BASE,
            vdev_virtio_net::VIRTIO_NET_SIZE,
        ),
    ];

    let Some(gm) = vm.guest_mem_mut() else {
        log::error!("[kvmm-api] guest_mem_mut unavailable for MMIO unmap");
        return Err(VfsError::NoSuchDevice);
    };

    for (name, mmio_base, size) in ranges {
        if !gm.unmap_range(mmio_base, size) {
            log::error!(
                "[kvmm-api] failed to unmap MMIO range {} @ {:#x}+{:#x}",
                name,
                mmio_base,
                size,
            );
            return Err(VfsError::NoSuchDevice);
        }
    }
    Ok(())
}

#[cfg(target_arch = "riscv64")]
fn setup_rv64_devices(vm: &Vm<CurrentArch>, console: &Arc<vdev_uart16550::Uart16550>) {
    use alloc::boxed::Box;

    use kvmm::vdev::{
        IrqController, IrqSender, VcpuWaker,
        riscv64::{irq, timer},
    };

    let waker = Arc::downgrade(&(vm.shared().clone() as Arc<dyn VcpuWaker>));
    let irqc = irq::RiscvIrq::new(vm.shared().nr_vcpus(), waker);
    vm.shared()
        .devices()
        .register_mmio(Box::new(irq::RiscvPlicMmio::new(irqc.clone())));
    vm.shared()
        .devices()
        .set_irq_controller(irqc.clone() as Arc<dyn IrqController>);
    vm.shared()
        .devices()
        .set_irq_sender(irqc.clone() as Arc<dyn IrqSender>);
    // Route the UART line: TX interrupts come from the UART itself, RX
    // interrupts from the console push path (see `VmDevices::push_console`).
    console.attach_irq(irqc.clone() as Arc<dyn IrqSender>, 0);
    vm.shared()
        .devices()
        .set_console_irq(vdev_uart16550::UART_IRQ);
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(irq::RiscvIrqHookFactory::new(irqc)));
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(timer::RiscvTimerHookFactory));
    log::info!("[kvmm-api] RISC-V IRQ/timer devices wired");
}

/// Load an aarch64 Linux Image, DTB, and optional initrd, then start vCPU0.
///
/// Configures the aarch64 Linux virtual platform: PL011 console, GICv2
/// (emulated GICD + GICC→GICV passthrough + host-only GICH), and the aarch64
/// vCPU entry state (`elr`/`spsr`/`x0=dtb`).
#[cfg(target_arch = "aarch64")]
fn build_and_start_aarch64(
    vm_id: u32,
    base: u64,
    kernel_path: &str,
    dtb_path: &str,
    initrd_path: Option<&str>,
) -> VfsResult<Vm<CurrentArch>> {
    use alloc::boxed::Box;

    use kvmm::{VmConfig, arch::VmmArch};

    /// Linux Image entry offset from RAM base.
    const LINUX_KERNEL_OFF: u64 = 0x0008_0000;
    /// DTB load offset from RAM base.
    const LINUX_DTB_OFF: u64 = 0x0400_0000;
    /// initrd load offset from RAM base.
    const LINUX_INITRD_OFF: u64 = 0x0800_0000;
    /// Total guest RAM per VM.
    const MEM_SIZE: u64 = 0x0C00_0000;

    let kernel_load = base + LINUX_KERNEL_OFF;
    let dtb_load = base + LINUX_DTB_OFF;
    let initrd_load = base + LINUX_INITRD_OFF;

    let nr_vcpus = loader::peek_dtb_cpu_count(dtb_path)
        .unwrap_or_else(|err| {
            log::warn!(
                "[kvmm-api] bootlinux: peek {:?}: {:?}, defaulting to 1 vCPU",
                dtb_path,
                err,
            );
            1
        })
        .max(1);

    let cfg = VmConfig::new(base, MEM_SIZE, nr_vcpus);
    let mut vm: Vm<CurrentArch> = Vm::new(cfg).ok_or(VfsError::NoMemory)?;

    {
        let gm = vm.shared().guest_mem().ok_or(VfsError::NoSuchDevice)?;
        let kernel_size =
            loader::load_image_to_guest(gm, kernel_path, kernel_load).map_err(|err| {
                log::error!(
                    "[kvmm-api] bootlinux: load kernel {:?}: {:?}",
                    kernel_path,
                    err
                );
                VfsError::InvalidInput
            })?;
        let dtb_size = loader::load_image_to_guest(gm, dtb_path, dtb_load).map_err(|err| {
            log::error!("[kvmm-api] bootlinux: load dtb {:?}: {:?}", dtb_path, err);
            VfsError::InvalidInput
        })?;
        loader::patch_dtb_memory(gm, dtb_load, dtb_size, base, MEM_SIZE).map_err(|err| {
            log::error!("[kvmm-api] bootlinux: patch memory: {:?}", err);
            VfsError::InvalidInput
        })?;
        loader::nop_dtb_nodes(
            gm,
            dtb_load,
            dtb_size,
            &["gpio-keys", "pl061@9030000", "v2m@8020000"],
        )
        .map_err(|err| {
            log::error!("[kvmm-api] bootlinux: patch unsupported nodes: {:?}", err);
            VfsError::InvalidInput
        })?;

        if let Some(path) = initrd_path {
            let initrd_size =
                loader::load_image_to_guest(gm, path, initrd_load).map_err(|err| {
                    log::error!("[kvmm-api] bootlinux: load initrd {:?}: {:?}", path, err);
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
                log::error!("[kvmm-api] bootlinux: patch initrd: {:?}", err);
                VfsError::InvalidInput
            })?;
            log::info!(
                "[kvmm-api] bootlinux: initrd {} bytes @ {:#x}",
                initrd_size,
                initrd_load,
            );
        }

        log::info!(
            "[kvmm-api] bootlinux: kernel {} bytes @ {:#x}, dtb {} bytes @ {:#x}",
            kernel_size,
            kernel_load,
            dtb_size,
            dtb_load,
        );
    }

    // Console PL011 + its RX/TX channels. The PL011 is itself the MmioDevice
    // (no separate front-end wrapper as on riscv64). Installing the TX channel
    // switches guest output from the host kernel log into the channel drained
    // by this device's `read`.
    {
        let (uart, rx, tx) = vdev_vpl011::Vpl011::new(vm_id);
        vm.shared().devices().set_console_rx(rx);
        vm.shared().devices().set_console_tx(tx);
        vm.shared().devices().register_mmio(Box::new(uart));
    }

    // Unmap emulated MMIO ranges so guest accesses trap into the VMM.
    unmap_aarch64_mmio(&mut vm)?;

    // GIC: emulated GICD, GICC→GICV passthrough, host-only GICH. Must run while
    // the shared Arc is still unique (before create_vcpu clones it).
    setup_gic(&mut vm)?;

    if let Some(irq_sender) = vm.shared().devices().irq_sender() {
        let dma = kvmm::vdev::dma::make_guest_dma(Arc::clone(vm.shared()));
        vm.shared()
            .devices()
            .register_mmio(Box::new(vdev_virtio_net::VirtioNet::new(
                vm_id, irq_sender, dma,
            )));
    }

    if !CurrentArch::percpu_hw_init() {
        log::error!("[kvmm-api] bootlinux: per-CPU HW init failed");
        return Err(VfsError::NoSuchDevice);
    }

    let mut vcpu = vm.create_vcpu(0);
    vcpu.arch.elr = kernel_load;
    vcpu.arch.sp_el1 = 0;
    vcpu.arch.spsr = 0x5 | (0xF << 6); // EL1h, DAIF masked
    vcpu.arch.gprs[0] = dtb_load; // x0: DTB physical address

    vm.shared().try_mark_cpu_on(0);
    vm.register();
    // The returned handle is dropped: the vCPU task is also published in the
    // VM's `vcpu_tasks` slot, which keeps the thread alive and lets
    // `stop_and_join` reclaim it at teardown.
    let _task = kvmm::spawn_vcpu_thread::<CurrentArch>(vcpu);

    log::info!(
        "[kvmm-api] bootlinux: aarch64 vm={} started vcpus={} entry={:#x} dtb={:#x}",
        vm_id,
        nr_vcpus,
        kernel_load,
        dtb_load,
    );
    Ok(vm)
}

/// Unmap every registered emulated MMIO range from second-stage translation so
/// guest accesses fault out to the VMM.
#[cfg(target_arch = "aarch64")]
fn unmap_aarch64_mmio(vm: &mut Vm<CurrentArch>) -> VfsResult<()> {
    use kvmm::mm::GuestMem;

    let ranges = vm.shared().devices().mmio_ranges();
    let Some(gm) = vm.guest_mem_mut() else {
        log::error!("[kvmm-api] guest_mem_mut unavailable for MMIO unmap");
        return Err(VfsError::NoSuchDevice);
    };

    for (name, mmio_base, size) in ranges {
        if !gm.unmap_range(mmio_base, size) {
            log::error!(
                "[kvmm-api] failed to unmap MMIO range {} @ {:#x}+{:#x}",
                name,
                mmio_base,
                size,
            );
            return Err(VfsError::NoSuchDevice);
        }
    }
    Ok(())
}

/// Wire up the AArch64 GIC for a VM: emulate the distributor (GICD), pass the
/// CPU interface (GICC) through to the hardware virtual interface (GICV), and
/// create the vGIC (GICH list-register injector).
#[cfg(target_arch = "aarch64")]
fn setup_gic(vm: &mut Vm<CurrentArch>) -> VfsResult<()> {
    use alloc::boxed::Box;

    use kvmm::{
        mm::{GuestMem, GuestPerm},
        vdev::{
            IrqController, IrqSender, VcpuWaker,
            aarch64::{vgic, vgicd, vtimer},
        },
    };

    const GICC_BASE: u64 = 0x0801_0000;
    const GICH_BASE: u64 = 0x0803_0000;
    const GICV_BASE: u64 = 0x0804_0000;
    const GIC_IF_SIZE: u64 = 0x1_0000;

    // GICC (guest) → GICV (hardware). Must run while the shared Arc is unique
    // (before create_vcpu clones it), since guest_mem_mut needs Arc::get_mut.
    match vm.guest_mem_mut() {
        Some(gm) => {
            if !gm.map_region(GICC_BASE, GICV_BASE, GIC_IF_SIZE, GuestPerm::DeviceRW) {
                log::error!("[kvmm-api] GICC→GICV map_region failed");
                return Err(VfsError::NoSuchDevice);
            }
        }
        None => {
            log::error!("[kvmm-api] guest_mem_mut unavailable for GIC map");
            return Err(VfsError::NoSuchDevice);
        }
    }

    // Map GICH (host-only) for the vGIC to program list registers.
    let gich_va = memspace::iomap_device(
        memaddr::PhysAddr::from(GICH_BASE as usize),
        0x1000,
        "kvmm-api-gich",
    )
    .map_err(|e| {
        log::error!("[kvmm-api] GICH iomap failed: {:?}", e);
        VfsError::NoSuchDevice
    })?;

    let waker = Arc::downgrade(&(vm.shared().clone() as Arc<dyn VcpuWaker>));
    let vgic = vgic::Vgic::new(vm.shared().nr_vcpus(), gich_va.as_usize(), waker);
    let vgicd = vgicd::Vgicd::new(vgic.clone(), vm.shared().nr_vcpus());
    vm.shared().devices().register_mmio(Box::new(vgicd));
    vm.shared()
        .devices()
        .set_irq_controller(vgic.clone() as Arc<dyn IrqController>);
    vm.shared()
        .devices()
        .set_irq_sender(vgic.clone() as Arc<dyn IrqSender>);
    // Route the PL011 RX line (SPI 1 = INTID 33). The amba-pl011 driver reads
    // the interactive tty through the RX interrupt, so the console push path
    // must inject it once the guest enables RXIM; without this, login stalls
    // after the username (the `Password:` read never wakes).
    vm.shared()
        .devices()
        .set_console_irq(vdev_vpl011::PL011_IRQ);
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(vtimer::VtimerHookFactory));
    vm.shared()
        .devices()
        .add_hook_factory(Arc::new(vgic::VgicHookFactory::new(vgic)));
    log::info!(
        "[kvmm-api] GIC wired: GICD emulated, GICC→GICV, GICH@{:#x}",
        gich_va.as_usize()
    );
    Ok(())
}
