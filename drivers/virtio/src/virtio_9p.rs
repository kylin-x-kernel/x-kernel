// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO 9p driver adapter.
use alloc::string::{String, ToString};

use driver_base::{Device, DeviceKind, DriverResult};
use kspin::SpinNoIrq;
use virtio_drivers::{Hal, device::virtio_9p::VirtIO9p as InnerDev, transport::Transport};

use crate::as_driver_error;

/// 9P-over-VirtIO device operations.
///
/// Defines the abstract interface used by the 9P filesystem layer so that the
/// concrete `VirtIo9pDev<H, T>` does not have to leak through subsystem APIs.
pub trait Virtio9pDevice: Device {
    /// Returns the mount tag reported by the device.
    fn mount_tag(&self) -> String;

    /// Sends a raw 9p request and waits for the response.
    fn request(&self, req: &[u8], resp: &mut [u8]) -> DriverResult<usize>;
}

/// The VirtIO 9p device driver.
///
/// Wraps `VirtIO9p` from `virtio-drivers` and implements the
/// [`Virtio9pDevice`] trait, providing a simple request/response
/// interface for 9p filesystem protocol communication between guest and host.
///
/// # Type Parameters
///
/// - `H` - VirtIO HAL implementation for DMA allocation.
/// - `T` - Transport layer (MMIO or PCI).
///
/// # Example
///
/// ```ignore
/// use virtio::Virtio9pDevice;
/// let (kind, transport) = virtio::probe_pci_device::<HalImpl, _>(...).unwrap();
/// let mut dev = VirtIo9pDev::<HalImpl, _>::try_new(transport)?;
/// let tag = dev.mount_tag();
/// let mut resp = [0u8; 256];
/// let n = dev.request(&req_buf, &mut resp)?;
/// ```
pub struct VirtIo9pDev<H: Hal, T: Transport> {
    inner: SpinNoIrq<InnerDev<H, T>>,
    mount_tag: String,
}

// SAFETY: VirtIo9pDev serializes access to the inner VirtIO9p through its own
// `SpinNoIrq` lock. The inner type is not auto Send/Sync due to PhantomData,
// but it is safe to transfer across threads and share behind that lock.
unsafe impl<H: Hal, T: Transport> Send for VirtIo9pDev<H, T> {}
// SAFETY: shared references to VirtIo9pDev still funnel all device interaction
// through the `SpinNoIrq` lock, preserving synchronized access.
unsafe impl<H: Hal, T: Transport> Sync for VirtIo9pDev<H, T> {}

impl<H: Hal, T: Transport> VirtIo9pDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    ///
    /// # Errors
    ///
    /// Returns `DriverError` if the VirtIO 9p device fails to initialize.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let inner = InnerDev::new(transport).map_err(as_driver_error)?;
        let mount_tag = inner.mount_tag().to_string();
        Ok(Self {
            inner: SpinNoIrq::new(inner),
            mount_tag,
        })
    }

    /// Returns the mount tag reported by the device.
    pub fn mount_tag(&self) -> String {
        self.mount_tag.clone()
    }
}

impl<H: Hal, T: Transport> Virtio9pDevice for VirtIo9pDev<H, T> {
    fn mount_tag(&self) -> String {
        self.mount_tag.clone()
    }

    /// Sends a raw 9p request and waits for the response.
    ///
    /// # Arguments
    ///
    /// - `req` - The raw 9p request bytes to send.
    /// - `resp` - Buffer to receive the response bytes.
    ///
    /// # Returns
    ///
    /// The number of bytes written into `resp`.
    ///
    /// # Errors
    ///
    /// Returns `DriverError` if the request fails (device error, buffer too
    /// small, etc.).
    fn request(&self, req: &[u8], resp: &mut [u8]) -> DriverResult<usize> {
        self.inner
            .lock()
            .request(req, resp)
            .map(|written| written as usize)
            .map_err(as_driver_error)
    }
}

impl<H: Hal, T: Transport> Device for VirtIo9pDev<H, T> {
    fn name(&self) -> &str {
        "virtio-9p"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Fs9p
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::mock_virtio::{MockHal, MockTransport};

    #[def_test]
    fn test_virtio_9p_init_failure() {
        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::_9P;
        let dev = VirtIo9pDev::<MockHal, MockTransport>::try_new(transport);
        assert!(dev.is_err());
    }

    #[def_test]
    fn test_virtio_9p_mount_tag() {
        let mut transport = MockTransport::new();
        transport.device_type = virtio_drivers::transport::DeviceType::_9P;
        let tag = b"hostshare";
        let len = tag.len() as u16;
        {
            let mut config = transport.config_space.borrow_mut();
            config[0..2].copy_from_slice(&len.to_le_bytes());
            config[2..2 + tag.len()].copy_from_slice(tag);
        }

        let dev = VirtIo9pDev::<MockHal, MockTransport>::try_new(transport).unwrap();
        assert_eq!(dev.name(), "virtio-9p");
        assert_eq!(dev.device_kind(), DeviceKind::Fs9p);
        assert_eq!(dev.mount_tag(), "hostshare");
    }

    #[def_test]
    fn test_virtio_9p_concurrency_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIo9pDev<MockHal, MockTransport>>();
    }
}
