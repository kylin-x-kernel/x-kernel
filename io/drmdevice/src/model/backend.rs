// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The only hardware boundary of this crate.
//!
//! The object model above this trait owns ids, state and lifetime; the backend
//! owns pixels. Capabilities are answered by the same object that implements
//! them, so a capability cannot be advertised without a working call behind it.

use alloc::sync::Arc;

use display::{DisplayDevice, DisplayInfo, ScanoutFormat, ScanoutRect, ScanoutResource};
use kclass::{ClassDevice, DisplayDeviceImpl};

use super::error::{DrmError, DrmResult};

/// Driver-local id of one scanout resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct ResourceId(u32);

impl ResourceId {
    /// Wraps the numeric id handed to the backend.
    pub(crate) const fn new(raw: u32) -> Self {
        Self(raw)
    }

    /// The numeric id.
    pub(crate) const fn raw(self) -> u32 {
        self.0
    }
}

/// Geometry and pixel format of a resource to create.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ResourceSpec {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub pitch: u32,
    /// Memory layout.
    pub format: ScanoutFormat,
}

/// A rectangle in pixels, as `present` describes the updated area.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Rect {
    /// Left coordinate.
    pub x: u32,
    /// Top coordinate.
    pub y: u32,
    /// Width.
    pub width: u32,
    /// Height.
    pub height: u32,
}

/// What the bound hardware can actually do.
///
/// Only capabilities with a working call behind them are listed; command
/// submission (execbuf/virgl) and dma-buf are not implemented, so there is no
/// field to advertise them by accident.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct BackendCaps {
    /// Scanout resources and dumb buffers are supported.
    pub dumb: bool,
    /// The device offers a 3D command channel (`virgl`).
    pub virgl: bool,
    /// The device negotiated per-file context initialization.
    pub context_init: bool,
    /// The device negotiated host-visible blob resources.
    pub blob: bool,
}

impl Rect {
    /// Converts to the display contract's rectangle.
    pub(crate) const fn to_scanout(self) -> ScanoutRect {
        ScanoutRect {
            x: self.x,
            y: self.y,
            width: self.width,
            height: self.height,
        }
    }
}

/// One display device behind a DRM card.
pub(crate) trait Backend: Send + Sync {
    /// Whether the underlying class device still exists.
    fn is_available(&self) -> bool;

    /// Visible mode of the display.
    fn info(&self) -> DisplayInfo;

    /// Identity of the concrete display driver.
    fn driver_info(&self) -> display::DriverInfo {
        display::DriverInfo::UNKNOWN
    }

    /// Re-reads the visible mode, reporting a host-side change.
    ///
    /// Returns the new mode when the display device observed one since the
    /// previous acknowledgement. Backends that cannot report changes keep the
    /// default, so `info` stays the only source of truth.
    fn refresh_info(&self) -> Option<DisplayInfo> {
        None
    }

    /// Capabilities that this backend implements.
    fn caps(&self) -> BackendCaps;

    /// The output's EDID bytes, when the device reports them.
    fn edid(&self) -> Option<alloc::vec::Vec<u8>> {
        None
    }

    /// Whether the output is connected right now.
    fn is_connected(&self) -> bool {
        let info = self.info();
        info.width != 0 && info.height != 0
    }

    /// The device's 3D command channel, when it has one.
    ///
    /// Kept apart from the rest of the trait on purpose: a 2D-only device is a
    /// complete `Backend`, and the virtio-gpu commands exist only when the
    /// feature was negotiated.
    fn gpu(&self) -> Option<&dyn display::GpuCommand> {
        None
    }

    /// Creates one host-visible resource backed by `length_bytes` at `paddr`.
    ///
    /// # Safety
    ///
    /// `paddr..paddr + length_bytes` must be a live, unshared window owned by
    /// the caller for as long as the resource exists.
    fn create_resource(
        &self,
        id: ResourceId,
        spec: ResourceSpec,
        window: &display::ScanoutWindow<'_>,
    ) -> DrmResult<()>;

    /// Destroys a resource created by [`Backend::create_resource`].
    fn destroy_resource(&self, id: ResourceId) -> DrmResult<()>;

    /// Makes `id` the active scanout and reports `rect` as updated.
    ///
    /// Either the display now scans the new resource, or the call failed and
    /// the previous scanout is still active, so callers can roll back.
    ///
    /// `host_rendered` says the host owns the pixels (a 3D/virgl resource):
    /// its guest pages must not be pushed over the host copy.
    fn present(&self, id: ResourceId, rect: Rect, host_rendered: bool) -> DrmResult<()>;

    /// Pushes updated pixels of an already-bound resource to the host.
    ///
    /// This is the dirty-framebuffer operation: it must not change which
    /// resource the scanout is bound to.
    fn flush(&self, id: ResourceId, rect: Rect, host_rendered: bool) -> DrmResult<()>;

    /// The display's blanking source, when it reports one.
    ///
    /// Without it the card has no vblank timeline, exactly as a Linux CRTC
    /// without `enable_vblank`.
    fn vblank_source(&self) -> Option<&dyn display::VblankSource> {
        None
    }
}

/// [`Backend`] over the `display` contract crate.
pub(crate) struct ClassBackend {
    display: ClassDevice<DisplayDeviceImpl>,
}

impl ClassBackend {
    /// Binds the backend to one published display device.
    pub(crate) const fn new(display: ClassDevice<DisplayDeviceImpl>) -> Self {
        Self { display }
    }
}

impl Backend for ClassBackend {
    fn vblank_source(&self) -> Option<&dyn display::VblankSource> {
        // The class handle owns the driver object that implements the source,
        // and this backend owns the class handle; the borrow is therefore
        // valid for as long as the card keeps the backend.
        self.display.select(
            |device| device.vblank_source(),
            |source| source.map(|source| source as &dyn display::VblankSource),
        )
    }

    fn is_available(&self) -> bool {
        self.display.is_available()
    }

    fn info(&self) -> DisplayInfo {
        self.display.info()
    }

    fn driver_info(&self) -> display::DriverInfo {
        self.display.driver_info()
    }

    fn refresh_info(&self) -> Option<DisplayInfo> {
        self.display.refresh_info()
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps {
            dumb: true,
            virgl: self
                .display
                .select(|device| device.gpu(), |gpu| gpu.is_some()),
            context_init: self.display.with(|device| device.has_context_init()),
            blob: self.display.with(|device| device.has_resource_blob()),
        }
    }

    fn edid(&self) -> Option<alloc::vec::Vec<u8>> {
        self.display.with(|device| device.edid())
    }

    fn is_connected(&self) -> bool {
        self.display.with(|device| device.is_connected())
    }

    fn gpu(&self) -> Option<&dyn display::GpuCommand> {
        // The class handle owns the driver object that implements the channel,
        // and this backend owns the class handle, so the borrow is valid for
        // as long as the card keeps the backend.
        self.display.select(
            |device| device.gpu(),
            |gpu| gpu.map(|gpu| gpu as &dyn display::GpuCommand),
        )
    }

    fn create_resource(
        &self,
        id: ResourceId,
        spec: ResourceSpec,
        window: &display::ScanoutWindow<'_>,
    ) -> DrmResult<()> {
        let resource = ScanoutResource {
            id: id.raw(),
            width: spec.width,
            height: spec.height,
            pitch: spec.pitch,
            format: spec.format,
        };
        self.display
            .create_scanout_resource(resource, window)
            .map_err(|_| DrmError::Io)
    }

    fn destroy_resource(&self, id: ResourceId) -> DrmResult<()> {
        self.display
            .destroy_scanout_resource(id.raw())
            .map_err(|_| DrmError::Io)
    }

    fn present(&self, id: ResourceId, rect: Rect, host_rendered: bool) -> DrmResult<()> {
        self.display
            .present_scanout_resource(id.raw(), rect.to_scanout(), host_rendered)
            .map_err(|_| DrmError::Io)
    }

    fn flush(&self, id: ResourceId, rect: Rect, host_rendered: bool) -> DrmResult<()> {
        self.display
            .flush_scanout_resource(id.raw(), rect.to_scanout(), host_rendered)
            .map_err(|_| DrmError::Io)
    }
}

/// Type-erased backend handle.
pub(crate) type BackendHandle = Arc<dyn Backend>;
