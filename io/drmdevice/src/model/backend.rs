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

    /// Creates one host-visible resource backed by `length_bytes` at `paddr`.
    fn create_resource(
        &self,
        id: ResourceId,
        spec: ResourceSpec,
        paddr: u64,
        length_bytes: u32,
    ) -> DrmResult<()>;

    /// Destroys a resource created by [`Backend::create_resource`].
    fn destroy_resource(&self, id: ResourceId) -> DrmResult<()>;

    /// Makes `id` the active scanout and reports `rect` as updated.
    ///
    /// Either the display now scans the new resource, or the call failed and
    /// the previous scanout is still active, so callers can roll back.
    fn present(&self, id: ResourceId, rect: Rect) -> DrmResult<()>;

    /// Pushes updated pixels of an already-bound resource to the host.
    ///
    /// This is the dirty-framebuffer operation: it must not change which
    /// resource the scanout is bound to.
    fn flush(&self, id: ResourceId, rect: Rect) -> DrmResult<()>;
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
    fn is_available(&self) -> bool {
        self.display.is_available()
    }

    fn info(&self) -> DisplayInfo {
        self.display.info()
    }

    fn refresh_info(&self) -> Option<DisplayInfo> {
        self.display.refresh_info()
    }

    fn caps(&self) -> BackendCaps {
        BackendCaps { dumb: true }
    }

    fn create_resource(
        &self,
        id: ResourceId,
        spec: ResourceSpec,
        paddr: u64,
        length_bytes: u32,
    ) -> DrmResult<()> {
        let resource = ScanoutResource {
            id: id.raw(),
            width: spec.width,
            height: spec.height,
            pitch: spec.pitch,
            format: spec.format,
        };
        self.display
            .create_scanout_resource(resource, paddr, length_bytes)
            .map_err(|_| DrmError::Io)
    }

    fn destroy_resource(&self, id: ResourceId) -> DrmResult<()> {
        self.display
            .destroy_scanout_resource(id.raw())
            .map_err(|_| DrmError::Io)
    }

    fn present(&self, id: ResourceId, rect: Rect) -> DrmResult<()> {
        self.display
            .present_scanout_resource(id.raw(), rect.to_scanout())
            .map_err(|_| DrmError::Io)
    }

    fn flush(&self, id: ResourceId, rect: Rect) -> DrmResult<()> {
        self.display
            .flush_scanout_resource(id.raw(), rect.to_scanout())
            .map_err(|_| DrmError::Io)
    }
}

/// Type-erased backend handle.
pub(crate) type BackendHandle = Arc<dyn Backend>;
