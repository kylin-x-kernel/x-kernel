// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wrappers of some devices in the [`virtio-drivers`] crate, that implement
//! traits in the [`driver_base`] series crates.
//!
//! Like the [`virtio-drivers`] crate, you must implement the [`VirtIoHal`]
//! trait (alias of `virtio_drivers::Hal`), to allocate DMA regions and
//! translate between physical addresses (as seen by devices) and virtual
//! addresses (as seen by your program).
//!
//! [`virtio-drivers`]: https://docs.rs/virtio-drivers/latest/virtio_drivers/
//! [`driver_base`]: https://docs.rs/virtio-drivers/latest/virtio_drivers/trait.Hal.html

#![no_std]
#![cfg_attr(doc, feature(doc_cfg))]

#[cfg(feature = "alloc")]
extern crate alloc;
#[cfg(feature = "net")]
extern crate net as driver_net;

#[cfg(feature = "block")]
mod blk;
#[cfg(feature = "block")]
pub use self::blk::VirtIoBlkDev;

#[cfg(feature = "gpu")]
mod gpu;
#[cfg(feature = "gpu")]
pub use self::gpu::VirtIoGpuDev;

#[cfg(feature = "input")]
mod input;
#[cfg(feature = "input")]
pub use self::input::VirtIoInputDev;

#[cfg(feature = "net")]
mod net;
#[cfg(feature = "net")]
pub use self::net::VirtIoNetDev;

#[cfg(feature = "virtio_9p")]
mod virtio_9p;
#[cfg(feature = "virtio_9p")]
pub use self::virtio_9p::VirtIo9pDev;

#[cfg(unittest)]
pub mod mock_virtio;
#[cfg(feature = "socket")]
mod socket;
use driver_base::{DeviceKind, DriverError};
use virtio_drivers::transport::{
    DeviceType as VirtIoDevType,
    pci::bus::{ConfigurationAccess, DeviceFunction, DeviceFunctionInfo, PciRoot},
};
pub use virtio_drivers::{
    BufferDirection, Hal as VirtIoHal, PhysAddr,
    transport::{Transport, mmio::MmioTransport, pci::PciTransport},
};

#[cfg(feature = "socket")]
pub use self::socket::VirtIoSocketDev;

/// Try to probe a VirtIO MMIO device from the given memory region.
///
/// If the device is recognized, returns the device type and a transport object
/// for later operations. Otherwise, returns [`None`].
///
/// # Arguments
///
/// - `reg_base` - Pointer to the MMIO register base of the device.
/// - `reg_size` - Size of the MMIO register region in bytes.
///
/// # Returns
///
/// `Some((DeviceKind, MmioTransport))` if the device is a recognized VirtIO
/// device, or `None` if the region does not contain a valid VirtIO header.
///
/// # Safety
///
/// The caller must ensure `reg_base` points to a valid VirtIO MMIO register
/// region of at least `reg_size` bytes, and that the memory remains valid
/// for the lifetime of the returned `MmioTransport`.
///
/// # Panics
///
/// Panics if `reg_base` is null.
pub unsafe fn probe_mmio_device(
    reg_base: *mut u8,
    reg_size: usize,
) -> Option<(DeviceKind, MmioTransport<'static>)> {
    use core::ptr::NonNull;

    use virtio_drivers::transport::mmio::VirtIOHeader;

    let header = NonNull::new(reg_base as *mut VirtIOHeader).unwrap();
    // SAFETY: The caller guarantees `reg_base` points to a valid MMIO region
    // of at least `reg_size` bytes. `MmioTransport::new` only reads the
    // header and validates the magic number and version.
    let transport = unsafe { MmioTransport::new(header, reg_size) }.ok()?;
    let dev_kind = as_device_kind(transport.device_type())?;
    Some((dev_kind, transport))
}

/// Try to probe a VirtIO PCI device from the given PCI address.
///
/// If the device is recognized, returns the device type, a transport object,
/// and the IRQ number for later operations. Otherwise, returns [`None`].
///
/// # Arguments
///
/// - `root` - Mutable reference to the PCI root bridge.
/// - `bdf` - Bus-Device-Function address of the PCI device.
/// - `dev_info` - Pre-read device function info (vendor/device ID, class, etc.).
/// - `config` - Mutable reference to PCI config space access helper.
///
/// # Returns
///
/// `Some((DeviceKind, PciTransport, irq))` on success, or `None` if the device
/// is not a recognized VirtIO device or transport creation fails.
///
/// # Errors
///
/// Returns `None` (not `Err`) if any step fails; the caller should try the next
/// device or fall back.
///
/// # Type Parameters
///
/// - `H` - VirtIO HAL implementation for DMA allocation.
/// - `C` - PCI configuration access implementation.
pub fn probe_pci_device<H: VirtIoHal, C: ConfigurationAccess>(
    root: &mut PciRoot<C>,
    bdf: DeviceFunction,
    dev_info: &DeviceFunctionInfo,
    config: &mut ::pci::PciConfigAccess,
) -> Option<(DeviceKind, PciTransport, usize)> {
    use virtio_drivers::transport::pci::virtio_device_type;

    let dev_kind = virtio_device_type(dev_info).and_then(as_device_kind)?;

    // Attempt MSI-X setup before creating PciTransport (x86_64 only).
    // MSI-X gives each device its own edge-triggered vector, eliminating
    // shared level-triggered IRQ issues.
    #[cfg(target_arch = "x86_64")]
    let irq = {
        #[allow(unused_imports)]
        use ::pci::msix::{
            MsixTableEntry, configure_msix_entry, enable_msix, find_msix_capability,
        };
        #[allow(unused_imports)]
        use khal::irq::{alloc_msix_vector, current_apic_id};

        // TODO: after virtio-drivers supports multiple MSI-X vectors, we should allocate and
        // configure
        /*
        if let Some(cap) = find_msix_capability(root, config, bdf) {
            // Allocate a CPU vector for this device.
            if let Some(vector) = alloc_msix_vector() {
                // Get the BAR that holds the MSI-X table.
                let bar = root.bar_info(bdf, cap.table_bar).ok().and_then(|info| {
                    match info {
                        ::pci::BarInfo::Memory { address, .. } => Some(address as usize),
                        _ => None,
                    }
                });

                if let Some(bar_phys) = bar {
                    let table_virt =
                        khal::mem::p2v((bar_phys + cap.table_offset as usize).into());
                    let table_ptr =
                        table_virt.as_mut_ptr() as *mut MsixTableEntry;

                    let apic_id = current_apic_id();

                    // Configure MSI-X table entry 0.
                    unsafe { configure_msix_entry(table_ptr, 0, vector, apic_id) };

                    // Enable MSI-X and disable legacy INTx.
                    enable_msix(root, config, bdf, &cap);

                    log::info!(
                        "PCI virtio device at {:?}: MSI-X vector = {:#x}",
                        bdf,
                        vector
                    );
                    vector as usize
                } else {
                    // BAR not mapped; fall back to legacy IRQ.
                    legacy_irq_for_bdf(config, bdf)
                }
            } else {
                // No vectors left; fall back to legacy IRQ.
                legacy_irq_for_bdf(config, bdf)
            }
        } else {
            // Device has no MSI-X capability; use legacy INTx.
            legacy_irq_for_bdf(config, bdf)
        }
        */
        legacy_irq_for_bdf(config, bdf)
    };

    #[cfg(not(target_arch = "x86_64"))]
    let irq = { legacy_irq_for_bdf(config, bdf) };

    let transport = PciTransport::new::<H, C>(root, bdf).ok()?;
    log::info!("PCI virtio device at {:?}: IRQ = {}", bdf, irq);
    Some((dev_kind, transport, irq))
}

/// Reads the PCI Interrupt Line register (config space offset 0x3C) for the
/// given device and returns it as a legacy IRQ number.
///
/// Returns 0xFF if the register has not been programmed by firmware, which
/// means the device has no usable legacy IRQ assignment. The caller should
/// treat 0xFF as "no IRQ".
#[cfg(target_arch = "x86_64")]
fn legacy_irq_for_bdf(config: &::pci::PciConfigAccess, bdf: DeviceFunction) -> usize {
    let word = config.read_word(bdf, 0x3C);
    let irq_line = (word & 0xFF) as usize;
    if irq_line == 0xFF || irq_line == 0 {
        log::warn!(
            "PCI device {:?}: Interrupt Line not assigned ({:#x}), legacy IRQ unavailable",
            bdf,
            irq_line
        );
    }
    if irq_line == 0xFF || irq_line == 0 {
        irq_line
    } else {
        khal::irq::map(
            khal::irq::IrqDesc::new(irq_line, khal::irq::IrqTrigger::LevelLow)
                .with_controller(khal::irq::IrqControllerKind::IoApic)
                .with_domain(khal::irq::IO_APIC_DOMAIN),
        )
    }
}

#[cfg(not(target_arch = "x86_64"))]
fn legacy_irq_for_bdf(config: &::pci::PciConfigAccess, bdf: DeviceFunction) -> usize {
    if let Some(route) = ::pci::legacy_interrupt_route(config, bdf) {
        #[cfg(target_arch = "aarch64")]
        {
            let desc = match route.trigger {
                of::InterruptTrigger::EdgeRising => khal::irq::gic_edge_irq_desc(route.irq),
                of::InterruptTrigger::LevelHigh
                | of::InterruptTrigger::LevelLow
                | of::InterruptTrigger::EdgeFalling
                | of::InterruptTrigger::Unknown(_) => khal::irq::gic_level_irq_desc(route.irq),
            };
            let virq = khal::irq::map(desc);
            log::info!(
                "virtio PCI IRQ via device tree: bdf={:?} hwirq={} virq={} trigger={:?}",
                bdf,
                route.irq,
                virq,
                route.trigger
            );
            return virq;
        }
        #[cfg(target_arch = "riscv64")]
        {
            return khal::irq::map(khal::irq::plic_irq_desc(route.irq));
        }
        #[cfg(target_arch = "loongarch64")]
        {
            return route.irq;
        }
    }

    #[cfg(target_arch = "loongarch64")]
    const PCI_IRQ_BASE: usize = 0x10;
    #[cfg(target_arch = "aarch64")]
    const PCI_IRQ_BASE: usize = 0x23;
    #[cfg(target_arch = "riscv64")]
    const PCI_IRQ_BASE: usize = 0x20;
    let hwirq = PCI_IRQ_BASE + (bdf.device & 3) as usize;
    log::info!(
        "virtio PCI IRQ fallback: bdf={:?} hwirq={} base={:#x}",
        bdf,
        hwirq,
        PCI_IRQ_BASE
    );
    #[cfg(target_arch = "aarch64")]
    {
        khal::irq::map(khal::irq::gic_level_irq_desc(hwirq))
    }
    #[cfg(target_arch = "riscv64")]
    {
        khal::irq::map(khal::irq::plic_irq_desc(hwirq))
    }
    #[cfg(target_arch = "loongarch64")]
    {
        hwirq
    }
}

const fn as_device_kind(t: VirtIoDevType) -> Option<DeviceKind> {
    use VirtIoDevType::*;
    match t {
        Block => Some(DeviceKind::Block),
        Network => Some(DeviceKind::Net),
        GPU => Some(DeviceKind::Display),
        Input => Some(DeviceKind::Input),
        Socket => Some(DeviceKind::Vsock),
        _9P => Some(DeviceKind::Virtio9p),
        _ => None,
    }
}

#[allow(dead_code)]
pub(crate) const fn as_driver_error(e: virtio_drivers::Error) -> DriverError {
    use virtio_drivers::{Error::*, device::socket::SocketError::*};
    match e {
        QueueFull => DriverError::BadState,
        NotReady => DriverError::WouldBlock,
        WrongToken => DriverError::BadState,
        AlreadyUsed => DriverError::AlreadyExists,
        InvalidParam => DriverError::InvalidInput,
        DmaError => DriverError::NoMemory,
        IoError => DriverError::Io,
        Unsupported => DriverError::Unsupported,
        ConfigSpaceTooSmall => DriverError::BadState,
        ConfigSpaceMissing => DriverError::BadState,
        SocketDeviceError(e) => match e {
            ConnectionExists => DriverError::AlreadyExists,
            NotConnected => DriverError::BadState,
            InvalidOperation | InvalidNumber | UnknownOperation(_) => DriverError::InvalidInput,
            OutputBufferTooShort(_) | BufferTooShort | BufferTooLong(..) => {
                DriverError::InvalidInput
            }
            UnexpectedDataInPacket | PeerSocketShutdown => DriverError::Io,
            InsufficientBufferSpaceInPeer => DriverError::WouldBlock,
            RecycledWrongBuffer => DriverError::BadState,
        },
    }
}
