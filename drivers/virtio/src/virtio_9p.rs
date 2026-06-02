// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO 9p driver adapter.
use driver_base::{DeviceKind, DriverOps, DriverResult};
use virtio_drivers::{Hal, device::virtio_9p::VirtIO9p as InnerDev, transport::Transport};

use crate::as_driver_error;

/// The VirtIO 9p device driver.
///
/// Wraps [`VirtIO9p`] from `virtio-drivers`, providing a simple request/response
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
/// let (kind, transport) = virtio::probe_pci_device::<HalImpl, _>(...).unwrap();
/// let mut dev = VirtIo9pDev::<HalImpl, _>::try_new(transport)?;
/// let tag = dev.mount_tag();
/// let mut resp = [0u8; 256];
/// let n = dev.request(&req_buf, &mut resp)?;
/// ```
pub struct VirtIo9pDev<H: Hal, T: Transport> {
    inner: InnerDev<H, T>,
}

// SAFETY: VirtIo9pDev accesses the device exclusively through &mut self.
// The inner VirtIO9p is not auto Send/Sync due to PhantomData, but it is
// safe to transfer across threads and share immutable references.
unsafe impl<H: Hal, T: Transport> Send for VirtIo9pDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIo9pDev<H, T> {}

impl<H: Hal, T: Transport> VirtIo9pDev<H, T> {
    /// Creates a new driver instance and initializes the device, or returns
    /// an error if any step fails.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] if the VirtIO 9p device fails to initialize.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        Ok(Self {
            inner: InnerDev::new(transport).map_err(as_driver_error)?,
        })
    }

    /// Returns the mount tag reported by the device.
    pub fn mount_tag(&self) -> &str {
        self.inner.mount_tag()
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
    /// Returns [`DriverError`] if the request fails (device error, buffer too
    /// small, etc.).
    pub fn request(&mut self, req: &[u8], resp: &mut [u8]) -> DriverResult<usize> {
        self.inner
            .request(req, resp)
            .map(|written| written as usize)
            .map_err(as_driver_error)
    }
}

impl<H: Hal, T: Transport> DriverOps for VirtIo9pDev<H, T> {
    fn name(&self) -> &str {
        "virtio-9p"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Virtio9p
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
        assert_eq!(dev.device_kind(), DeviceKind::Virtio9p);
        assert_eq!(dev.mount_tag(), "hostshare");
    }

    #[def_test]
    fn test_virtio_9p_concurrency_traits() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<VirtIo9pDev<MockHal, MockTransport>>();
    }
}
