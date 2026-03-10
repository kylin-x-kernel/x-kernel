// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wrappers of some devices in the [`virtio-drivers`][1] crate, that implement
//! traits in the [`driver_base`][2] series crates.
//!
//! Like the [`virtio-drivers`][1] crate, you must implement the [`VirtIoHal`]
//! trait (alias of [`virtio-drivers::Hal`][3]), to allocate DMA regions and
//! translate between physical addresses (as seen by devices) and virtual
//! addresses (as seen by your program).
//!
//! [1]: https://docs.rs/virtio-drivers/latest/virtio_drivers/
//! [2]: https://docs.rs/virtio-drivers/latest/virtio_drivers/trait.Hal.html

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

#[cfg(unittest)]
pub mod mock_virtio;
#[cfg(feature = "socket")]
mod socket;
use driver_base::{DeviceKind, DriverError};
use virtio_drivers::transport::DeviceType as VirtIoDevType;
pub use virtio_drivers::{
    BufferDirection, Hal as VirtIoHal, PhysAddr,
    transport::{
        Transport,
        mmio::MmioTransport,
        pci::{PciTransport, bus as virtio_pci_bus},
    },
};

#[cfg(feature = "socket")]
pub use self::socket::VirtIoSocketDev;
use self::virtio_pci_bus::{DeviceFunction, DeviceFunctionInfo, PciRoot};

/// Try to probe a VirtIO MMIO device from the given memory region.
///
/// If the device is recognized, returns the device type and a transport object
/// for later operations. Otherwise, returns [`None`].
pub fn probe_mmio_device(
    reg_base: *mut u8,
    _reg_size: usize,
) -> Option<(DeviceKind, MmioTransport)> {
    use core::ptr::NonNull;

    use virtio_drivers::transport::mmio::VirtIOHeader;

    let header = NonNull::new(reg_base as *mut VirtIOHeader).unwrap();
    let transport = unsafe { MmioTransport::new(header) }.ok()?;
    let dev_kind = as_device_kind(transport.device_type())?;
    Some((dev_kind, transport))
}

/// Try to probe a VirtIO PCI device from the given PCI address.
///
/// If the device is recognized, returns the device type and a transport object
/// for later operations. Otherwise, returns [`None`].
pub fn probe_pci_device<H: VirtIoHal>(
    root: &mut PciRoot,
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
    let irq = {
        let _ = &config; // not used on non-x86_64 platforms
        #[cfg(target_arch = "loongarch64")]
        const PCI_IRQ_BASE: usize = 0x10;
        #[cfg(target_arch = "aarch64")]
        const PCI_IRQ_BASE: usize = 0x23;
        #[cfg(target_arch = "riscv64")]
        const PCI_IRQ_BASE: usize = 0x20;
        PCI_IRQ_BASE + (bdf.device & 3) as usize
    };

    let transport = PciTransport::new::<H>(root, bdf).ok()?;
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
    irq_line
}

const fn as_device_kind(t: VirtIoDevType) -> Option<DeviceKind> {
    use VirtIoDevType::*;
    match t {
        Block => Some(DeviceKind::Block),
        Network => Some(DeviceKind::Net),
        GPU => Some(DeviceKind::Display),
        Input => Some(DeviceKind::Input),
        Socket => Some(DeviceKind::Vsock),
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
            UnexpectedDataInPacket | PeerSocketShutdown | NoResponseReceived | ConnectionFailed => {
                DriverError::Io
            }
            InsufficientBufferSpaceInPeer => DriverError::WouldBlock,
            RecycledWrongBuffer => DriverError::BadState,
        },
    }
}
