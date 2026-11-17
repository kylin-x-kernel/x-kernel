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
//! [`driver_base`]: driver_base

#![allow(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): legacy virtio has 14 diagnostic \
              sites, too many to judge individually in this migration; new code remains reviewed"
)]
#![no_std]
#![allow(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): 11 legacy diagnostic sites need \
              individual failure-contract review; new code remains subject to the \
              no-legitimate-failure-panic rule"
)]
#![cfg_attr(doc, feature(doc_cfg))]

#[cfg(feature = "alloc")]
extern crate alloc;
#[cfg(feature = "net")]
extern crate net as driver_net;

#[cfg(feature = "block")]
mod blk;
#[cfg(feature = "block")]
pub use self::blk::{PART_BITS as VIRTIO_BLK_PART_BITS, VIRTIO_BLK_MAJOR, VirtIoBlkDev};

#[cfg(feature = "gpu")]
mod gpu;
#[cfg(feature = "gpu")]
pub use self::gpu::{GpuHost, VirtIoGpuDev};

#[cfg(feature = "input")]
mod input;
#[cfg(feature = "input")]
pub use self::input::VirtIoInputDev;

#[cfg(feature = "net")]
mod net;
#[cfg(feature = "net")]
pub use self::net::VirtIoNetDev;

#[cfg(feature = "rng")]
mod device_dma;
#[cfg(feature = "rng")]
pub use self::device_dma::DeviceDmaBackend;
#[cfg(feature = "rng")]
mod rng;
#[cfg(feature = "rng")]
pub use self::rng::VirtIoRngDev;

mod pci;
pub use self::pci::{PciProbeError, PciTransport, probe_pci_device};

#[cfg(feature = "virtio-9p")]
mod virtio_9p;
#[cfg(feature = "virtio-9p")]
pub use self::virtio_9p::{VirtIo9pDev, Virtio9pDevice};

#[cfg(unittest)]
pub mod mock_virtio;
#[cfg(feature = "socket")]
mod socket;
use core::ptr::NonNull;

use driver_base::{DeviceKind, DriverError};
use virtio_drivers::transport::{
    DeviceType as VirtIoDevType,
    mmio::{VirtIOHeader, read_mmio_header},
};
pub use virtio_drivers::{
    BufferDirection, Hal as VirtIoHal, PhysAddr, Result as VirtioResult,
    transport::{
        DeviceStatus, DeviceType, InterruptStatus, Transport,
        mmio::{MmioError, MmioHeaderInfo, MmioTransport, MmioVersion},
    },
};
use zerocopy::{FromBytes, Immutable, IntoBytes};

#[cfg(feature = "socket")]
pub use self::socket::VirtIoVsockDev;

/// Read-only identification of a VirtIO MMIO device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MmioIdentification {
    /// The version of the VirtIO MMIO transport supported by the device.
    pub version: MmioVersion,
    /// The raw VirtIO device ID reported by the device.
    pub device_id: u32,
    /// The driver-facing kind for `device_id`, or [`None`] if it is zero or
    /// not supported by this crate.
    pub kind: Option<DeviceKind>,
}

/// Identifies a VirtIO MMIO device from the given memory region.
///
/// This performs a read-only header check: it never writes a register, never
/// constructs a transport (whose `Drop` resets the device) and preserves zero
/// or unsupported device IDs as ordinary values with `kind: None`. Only
/// header/region problems are reported as errors.
///
/// # Arguments
///
/// - `reg_base` - Pointer to the MMIO register base of the device.
/// - `reg_size` - Size of the MMIO register region in bytes.
///
/// # Safety
///
/// The caller must ensure `reg_base` points to a valid, properly aligned MMIO
/// register region of at least `reg_size` bytes for the duration of the call,
/// and must have exclusive access to that region for the whole call: no other
/// mapping, transport or interrupt handler may read or write it concurrently.
/// The vendor reader creates a `UniqueMmioPointer` over the header, whose
/// contract forbids other aliases that access the same region while it exists.
pub unsafe fn identify_mmio_device(
    reg_base: NonNull<u8>,
    reg_size: usize,
) -> Result<MmioIdentification, MmioError> {
    // SAFETY: Forwarded from the caller.
    let header = unsafe { read_mmio_header(reg_base, reg_size) }?;
    Ok(MmioIdentification {
        version: header.version,
        device_id: header.device_id,
        kind: device_kind_for_id(header.device_id),
    })
}

/// Opens the transport for an already identified VirtIO MMIO device.
///
/// Unlike [`identify_mmio_device`], this constructs an `MmioTransport`: it
/// validates the device ID and its `Drop` implementation resets the device.
/// Call it only after identifying the device as one this driver handles.
///
/// # Safety
///
/// The caller must ensure `reg_base` points to a valid, properly aligned MMIO
/// register region of at least `reg_size` bytes, that the memory remains valid
/// for the lifetime of the returned `MmioTransport`, and that the caller has
/// exclusive access to the region for that lifetime: no other alias may access
/// the same registers while the transport exists.
pub unsafe fn open_mmio_transport(
    reg_base: NonNull<u8>,
    reg_size: usize,
) -> Result<MmioTransport<'static>, MmioError> {
    // SAFETY: Forwarded from the caller.
    unsafe { MmioTransport::new(reg_base.cast::<VirtIOHeader>(), reg_size) }
}

/// A [`Transport`] that keeps the resource owning its registers alive.
///
/// `transport` is declared before `_owner`, so it is dropped first: the
/// transport performs its final register access and its `Drop` reset, and only
/// then does the owner release the mapping. Callers that obtain a mapping from
/// a reference-counted `iomap` should wrap the opened transport and its mapping
/// guard here instead of deciding separately whether to unmap.
///
/// Dropping the final owner also releases any MMIO range claim it holds. That
/// release must run in normal, sleepable context with local IRQs enabled and
/// without a spin or preemption guard, so the last transport reference must not
/// be dropped from IRQ or atomic context.
///
/// `T` defaults to the production [`MmioTransport`]; unit tests substitute a
/// mock register backend while keeping the same ownership and drop order.
pub struct OwnedMmioTransport<G, T = MmioTransport<'static>> {
    transport: T,
    _owner: G,
}

impl<G, T> OwnedMmioTransport<G, T> {
    /// Wraps a transport together with the guard that owns its mapping.
    pub const fn new(transport: T, owner: G) -> Self {
        Self {
            transport,
            _owner: owner,
        }
    }
}

impl<G, Tr: Transport> Transport for OwnedMmioTransport<G, Tr> {
    fn device_type(&self) -> DeviceType {
        self.transport.device_type()
    }

    fn read_device_features(&mut self) -> u64 {
        self.transport.read_device_features()
    }

    fn write_driver_features(&mut self, driver_features: u64) {
        self.transport.write_driver_features(driver_features);
    }

    fn max_queue_size(&mut self, queue: u16) -> u32 {
        self.transport.max_queue_size(queue)
    }

    fn notify(&mut self, queue: u16) {
        self.transport.notify(queue);
    }

    fn get_status(&self) -> DeviceStatus {
        self.transport.get_status()
    }

    fn set_status(&mut self, status: DeviceStatus) {
        self.transport.set_status(status);
    }

    fn reset(&mut self) {
        // Forward explicitly so the wrapped transport's own reset semantics
        // (including its wait for the status readback) are preserved.
        self.transport.reset();
    }

    fn set_guest_page_size(&mut self, guest_page_size: u32) {
        self.transport.set_guest_page_size(guest_page_size);
    }

    fn requires_legacy_layout(&self) -> bool {
        self.transport.requires_legacy_layout()
    }

    fn queue_set(
        &mut self,
        queue: u16,
        size: u32,
        descriptors: PhysAddr,
        driver_area: PhysAddr,
        device_area: PhysAddr,
    ) {
        self.transport
            .queue_set(queue, size, descriptors, driver_area, device_area);
    }

    fn queue_unset(&mut self, queue: u16) {
        self.transport.queue_unset(queue);
    }

    fn queue_used(&mut self, queue: u16) -> bool {
        self.transport.queue_used(queue)
    }

    fn ack_interrupt(&mut self) -> InterruptStatus {
        self.transport.ack_interrupt()
    }

    fn read_config_generation(&self) -> u32 {
        self.transport.read_config_generation()
    }

    fn read_config_space<T: FromBytes + IntoBytes>(&self, offset: usize) -> VirtioResult<T> {
        self.transport.read_config_space(offset)
    }

    fn write_config_space<T: IntoBytes + Immutable>(
        &mut self,
        offset: usize,
        value: T,
    ) -> VirtioResult<()> {
        self.transport.write_config_space(offset, value)
    }
}

/// Maps a raw VirtIO device ID to the driver-facing device kind.
pub fn device_kind_for_id(device_id: u32) -> Option<DeviceKind> {
    VirtIoDevType::try_from(device_id)
        .ok()
        .and_then(as_device_kind)
}

const fn as_device_kind(t: VirtIoDevType) -> Option<DeviceKind> {
    use VirtIoDevType::*;
    match t {
        Block => Some(DeviceKind::Block),
        Network => Some(DeviceKind::Net),
        EntropySource => Some(DeviceKind::Hwrng),
        GPU => Some(DeviceKind::Display),
        Input => Some(DeviceKind::Input),
        Socket => Some(DeviceKind::Vsock),
        _9P => Some(DeviceKind::Fs9p),
        _ => None,
    }
}

unittest::path_mod_test!(mmio_unittest, "../ut-tests/mmio_unittest.rs");

#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Error conversion is used only by enabled VirtIO device \
              backends"
)]
#[allow(
    dead_code,
    reason = "CG-024-lint-exceptions: dead_code: Error conversion is used only by enabled VirtIO \
              device backends"
)]
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
