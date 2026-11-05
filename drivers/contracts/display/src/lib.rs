// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common traits and types for graphics display device drivers.

#![no_std]

#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};

/// The information of the graphics display device.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct DisplayInfo {
    /// The visible width.
    pub width: u32,
    /// The visible height.
    pub height: u32,
}

/// A 2D scanout rectangle.
#[derive(Debug, Clone, Copy, Default, Eq, PartialEq)]
pub struct ScanoutRect {
    /// The left coordinate in pixels.
    pub x: u32,
    /// The top coordinate in pixels.
    pub y: u32,
    /// The rectangle width in pixels.
    pub width: u32,
    /// The rectangle height in pixels.
    pub height: u32,
}

/// Pixel format used by scanout resources.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum ScanoutFormat {
    /// 32-bit BGRA/XRGB memory layout as consumed by virtio-gpu 2D resources.
    Bgra8888,
}

/// A host-visible 2D resource backed by guest memory.
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub struct ScanoutResource {
    /// Driver-local resource identifier.
    pub id: u32,
    /// Resource width in pixels.
    pub width: u32,
    /// Resource height in pixels.
    pub height: u32,
    /// Resource stride in bytes.
    pub pitch: u32,
    /// Resource pixel format.
    pub format: ScanoutFormat,
}

/// Operations that require a graphics device driver to implement.
pub trait DisplayDevice: Device {
    /// Get the display information.
    ///
    /// The returned size is the one the host currently scans out. Call
    /// [`Self::refresh_info`] first to pick up a host-side change.
    fn info(&self) -> DisplayInfo;

    /// Re-read the display information from the device.
    ///
    /// Returns the new information when the host reported a change since the
    /// last acknowledgement (for example the QEMU window was resized), and
    /// `None` when the cached information is still current. Implementations
    /// that cannot observe host changes keep the default, which never reports
    /// one.
    ///
    /// The call may issue device requests; callers use it on query paths, not
    /// on scanout presentation.
    fn refresh_info(&self) -> Option<DisplayInfo> {
        None
    }

    /// Whether need to flush the framebuffer to the screen.
    fn need_flush(&self) -> bool;

    /// Flush framebuffer to the screen.
    fn flush(&self) -> DriverResult;

    /// Create a host-visible 2D resource backed by guest memory.
    fn create_scanout_resource(
        &self,
        _resource: ScanoutResource,
        _paddr: u64,
        _length: u32,
    ) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Destroy a previously created scanout resource.
    fn destroy_scanout_resource(&self, _resource_id: u32) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Transfer resource contents to the host and make it the active scanout.
    fn present_scanout_resource(&self, _resource_id: u32, _rect: ScanoutRect) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Transfer updated pixels to the host and flush them, without changing
    /// which resource is bound to the scanout.
    ///
    /// This is the operation behind `DRM_IOCTL_MODE_DIRTYFB`: a client that
    /// rendered into the framebuffer that is already on screen asks for the
    /// pixels to be pushed, not for the scanout to be rebound.
    fn flush_scanout_resource(&self, _resource_id: u32, _rect: ScanoutRect) -> DriverResult {
        Err(DriverError::Unsupported)
    }
}

#[path = "../ut-tests/display_contract.rs"]
mod display_contract_tests;
