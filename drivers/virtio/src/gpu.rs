// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO GPU driver adapter.
use display::{DisplayDriverOps, DisplayInfo, FrameBuffer};
use driver_base::{DeviceKind, DriverOps, DriverResult};
use virtio_drivers::{Hal, device::gpu::VirtIOGpu as InnerDev, transport::Transport};

/// The VirtIO GPU device driver.
///
/// Wraps [`VirtIOGpu`] from `virtio-drivers` and implements the
/// [`DisplayDriverOps`] trait, providing framebuffer access and display
/// flush operations.
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
/// let mut gpu = VirtIoGpuDev::<HalImpl, _>::try_new(transport)?;
/// let info = gpu.info();
/// let fb = gpu.fb();
/// ```
pub struct VirtIoGpuDev<H: Hal, T: Transport> {
    info: DisplayInfo,
    inner: InnerDev<H, T>,
}

// SAFETY: VirtIoGpuDev accesses the device exclusively through &mut self.
// The inner VirtIOGpu is not auto Send/Sync due to PhantomData, but it is
// safe to transfer across threads and share immutable references.
unsafe impl<H: Hal, T: Transport> Send for VirtIoGpuDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoGpuDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
    /// Creates a new driver instance, sets up the framebuffer, and reads the
    /// display resolution.
    ///
    /// # Errors
    ///
    /// Returns [`DriverError`] if device initialization, framebuffer setup, or
    /// resolution query fails.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let mut device = InnerDev::new(transport).map_err(crate::as_driver_error)?;
        let framebuffer = device.setup_framebuffer().map_err(crate::as_driver_error)?;
        let fb_base_vaddr = framebuffer.as_mut_ptr() as usize;
        let fb_size = framebuffer.len();
        let (width, height) = device.resolution().map_err(crate::as_driver_error)?;

        Ok(Self {
            info: DisplayInfo {
                width,
                height,
                fb_base_vaddr,
                fb_size,
            },
            inner: device,
        })
    }
}

impl<H: Hal, T: Transport> DriverOps for VirtIoGpuDev<H, T> {
    fn name(&self) -> &str {
        "virtio-gpu"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Display
    }
}

impl<H: Hal, T: Transport> DisplayDriverOps for VirtIoGpuDev<H, T> {
    fn info(&self) -> DisplayInfo {
        self.info
    }

    fn fb(&self) -> FrameBuffer<'_> {
        // SAFETY: `fb_base_vaddr` and `fb_size` were obtained from
        // `setup_framebuffer()` during `try_new()`, which returns a valid
        // framebuffer slice. The lifetime is tied to `&self`, so the caller
        // cannot outlive the device. Additionally, `fb_base_vaddr` is stored
        // in `DisplayInfo` as a plain `usize`, but it is set once in
        // `try_new()` and never modified afterwards; `VirtIoGpuDev::inner`
        // is private, so external code cannot invalidate this pointer.
        unsafe {
            FrameBuffer::from_raw_parts_mut(self.info.fb_base_vaddr as *mut u8, self.info.fb_size)
        }
    }

    fn need_flush(&self) -> bool {
        true
    }

    fn flush(&mut self) -> DriverResult {
        self.inner.flush().map_err(crate::as_driver_error)
    }
}
