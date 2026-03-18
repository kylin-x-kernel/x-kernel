// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO GPU driver adapter.
use display::{DisplayDriverOps, DisplayInfo, FrameBuffer};
use driver_base::{DeviceKind, DriverOps, DriverResult};
use virtio_drivers::{Hal, device::gpu::VirtIOGpu as InnerDev, transport::Transport};

/// The VirtIO GPU device driver.
pub struct VirtIoGpuDev<H: Hal, T: Transport> {
    info: DisplayInfo,
    inner: InnerDev<H, T>,
}

unsafe impl<H: Hal, T: Transport> Send for VirtIoGpuDev<H, T> {}
unsafe impl<H: Hal, T: Transport> Sync for VirtIoGpuDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
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
