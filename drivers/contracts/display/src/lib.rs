// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common traits and types for graphics display device drivers.

#![no_std]

extern crate alloc;

pub use memaddr::{PhysAddr, PhysAddrRange};

mod dmabuf;
mod vblank;

pub use dmabuf::{
    DmaBufAttachment, DmaBufObject, DmaBufSlot, ExportedDmaBuf, FileSlotWriter, MappedMemory, find,
    publish,
};
#[doc(no_inline)]
pub use driver_base::{Device, DeviceKind, DriverError, DriverResult};
pub use vblank::{VblankOrigin, VblankSource, VblankTick};

/// Identity a display driver reports through DRM and dma-buf interfaces.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DriverInfo {
    /// Driver name (`drm_driver.name`).
    pub name: &'static str,
    /// Driver date (`drm_driver.date`).
    pub date: &'static str,
    /// Driver description (`drm_driver.desc`).
    pub desc: &'static str,
}

impl DriverInfo {
    /// Identity used when a backend does not provide one.
    pub const UNKNOWN: Self = Self {
        name: "unknown",
        date: "unknown",
        desc: "unknown display driver",
    };
}

/// Connector type name used by the virtual display.
///
/// Linux names a connector `<type>-<type_id>`; the DRM virtual connector is
/// `Virtual`, so the first one is `Virtual-1`.
pub const VIRTUAL_CONNECTOR_NAME: &str = "Virtual";
/// Type id of the first virtual connector.
pub const VIRTUAL_CONNECTOR_TYPE_ID: u32 = 1;

/// Formats a DRM connector name as `<type>-<type_id>`.
pub fn connector_name(type_name: &str, type_id: u32) -> alloc::string::String {
    alloc::format!("{type_name}-{type_id}")
}

/// The information of the graphics display device.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DisplayInfo {
    /// The visible width.
    pub width: u32,
    /// The visible height.
    pub height: u32,
}

/// Pending interrupt causes acknowledged by a display device.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct DisplayIrqStatus {
    /// A virtqueue buffer was used. A driver may wake its command waiter.
    pub queue: bool,
    /// The device configuration changed, for example the host resized the
    /// visible output.
    pub config: bool,
}

impl DisplayIrqStatus {
    /// Whether the device reported no cause for this interrupt.
    pub const fn is_empty(self) -> bool {
        !self.queue && !self.config
    }
}

/// Memory backing a scanout resource for the duration of the registration.
///
/// # Safety
///
/// The returned range must describe a live, unshared allocation owned by
/// `self`, and the allocation must remain alive until the display resource is
/// destroyed.
pub unsafe trait ScanoutBacking: Send + Sync {
    /// Physical range of the visible pixels.
    fn range(&self) -> PhysAddrRange;
}

/// A borrowed guarantee that the scanout memory stays alive during the call.
///
/// The backing is borrowed, not retained: the display driver uses the range to
/// program the host resource, while the caller keeps the backing alive for as
/// long as that host resource exists.
pub struct ScanoutWindow<'a> {
    range: PhysAddrRange,
    _backing: &'a dyn ScanoutBacking,
}

impl<'a> ScanoutWindow<'a> {
    /// Captures the backing range while borrowing the allocation.
    pub fn new(backing: &'a dyn ScanoutBacking) -> Self {
        Self {
            range: backing.range(),
            _backing: backing,
        }
    }

    /// Physical range of the visible pixels.
    pub const fn range(&self) -> PhysAddrRange {
        self.range
    }
}

/// A 2D scanout rectangle.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ScanoutFormat {
    /// 32-bit BGRA/XRGB memory layout as consumed by virtio-gpu 2D resources.
    Bgra8888,
}

/// A host-visible 2D resource backed by guest memory.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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

    /// Identity of the concrete display driver.
    fn driver_info(&self) -> DriverInfo {
        DriverInfo::UNKNOWN
    }

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

    /// Acknowledge the device interrupt source, returning its pending causes.
    ///
    /// A shared level-triggered line is only released after this read. The
    /// default reports no cause, so a driver that cannot acknowledge an
    /// interrupt must not register a hard-IRQ handler.
    fn ack_irq(&self) -> DisplayIrqStatus {
        DisplayIrqStatus::default()
    }

    /// Whether need to flush the framebuffer to the screen.
    fn need_flush(&self) -> bool;

    /// Flush framebuffer to the screen.
    fn flush(&self) -> DriverResult;

    /// Create a host-visible 2D resource backed by guest memory.
    ///
    /// # Safety
    ///
    /// `paddr..paddr + length` must be a live, unshared guest-memory window
    /// owned by the caller for as long as the resource exists.
    fn create_scanout_resource(
        &self,
        _resource: ScanoutResource,
        _window: &ScanoutWindow<'_>,
    ) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Destroy a previously created scanout resource.
    fn destroy_scanout_resource(&self, _resource_id: u32) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Transfer resource contents to the host and make it the active scanout.
    ///
    /// `host_rendered` marks a resource whose pixels the host produced (a 3D
    /// context rendered into it): its guest pages are not the source of truth,
    /// so the transfer is skipped and only the scanout binding and flush are
    /// performed.
    fn present_scanout_resource(
        &self,
        _resource_id: u32,
        _rect: ScanoutRect,
        _host_rendered: bool,
    ) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// Transfer updated pixels to the host and flush them, without changing
    /// which resource is bound to the scanout.
    ///
    /// This is the operation behind `DRM_IOCTL_MODE_DIRTYFB`: a client that
    /// rendered into the framebuffer that is already on screen asks for the
    /// pixels to be pushed, not for the scanout to be rebound.
    fn flush_scanout_resource(
        &self,
        _resource_id: u32,
        _rect: ScanoutRect,
        _host_rendered: bool,
    ) -> DriverResult {
        Err(DriverError::Unsupported)
    }

    /// The connector's EDID bytes, when the device has them.
    ///
    /// This is the descriptor a display reported for its output, base block
    /// first. A device that cannot read one returns `None`, and the consumer
    /// then publishes no `EDID` property instead of an invented descriptor.
    fn edid(&self) -> Option<alloc::vec::Vec<u8>> {
        None
    }

    /// Whether the output is connected right now.
    ///
    /// A device that always reports a mode is always connected; a device that
    /// can lose its output says so here and the consumer turns it into a
    /// connector status and a hotplug event.
    fn is_connected(&self) -> bool {
        self.info().width != 0 && self.info().height != 0
    }

    /// The 3D command channel, when the device negotiated one.
    ///
    /// A device without virgl returns `None`, and the consumer then advertises
    /// no 3D at all instead of accepting commands the host would reject.
    fn gpu(&self) -> Option<&dyn GpuCommand> {
        None
    }

    /// Whether the device negotiated host-visible blob resources.
    ///
    /// This is `VIRTIO_GPU_F_RESOURCE_BLOB`: without it a client must not be
    /// told that host memory can be shared or mapped.
    fn has_resource_blob(&self) -> bool {
        false
    }

    /// Whether the device negotiated per-file context initialization
    /// (`VIRTIO_GPU_F_CONTEXT_INIT`, `VIRTGPU_PARAM_CONTEXT_INIT`).
    fn has_context_init(&self) -> bool {
        false
    }

    /// Where this display reports its blanking intervals, when it can.
    ///
    /// A display that observes its blanking returns itself; one that cannot
    /// returns `None`, and the consumer then has no vblank timeline rather
    /// than a synthesized one.
    fn vblank_source(&self) -> Option<&dyn VblankSource> {
        None
    }
}

/// The fence the host signals once it processed a command stream.
pub type GpuFenceId = u64;

/// One capability set the device offers (`virgl`, `virgl2`, ...).
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct CapsetInfo {
    /// Capset identifier (`VIRTIO_GPU_CAPSET_VIRGL` is 1, `VIRGL2` is 2).
    pub id: u32,
    /// Highest version the device supports.
    pub max_version: u32,
    /// Largest capset blob the device returns in one request.
    pub max_size: u32,
}

/// The 3D command channel of a device (`virtio-gpu`'s VIRGL feature).
///
/// Every operation maps to one control command of the virtio-gpu spec. The
/// consumer owns the userspace ABI; this trait is the boundary it talks to,
/// and each call returns the device's answer without interpreting it.
pub trait GpuCommand {
    /// Whether the device implements this channel.
    fn is_available(&self) -> bool;

    /// Queries capset `index` (0-based), or `None` past the last one.
    fn capset_info(&self, index: u32) -> DriverResult<Option<CapsetInfo>>;

    /// Reads the capset blob of `id` at `version`, up to `size` bytes.
    fn capset(&self, id: u32, version: u32, size: u32) -> DriverResult<alloc::vec::Vec<u8>>;

    /// Creates a 3D context `ctx_id`, named `name`, with `context_init`.
    fn create_context(&self, ctx_id: u32, name: &str, context_init: u32) -> DriverResult;

    /// Destroys a 3D context.
    fn destroy_context(&self, ctx_id: u32) -> DriverResult;

    /// Creates a 3D resource in `ctx_id`.
    ///
    /// The arguments are the virtio-gpu `RESOURCE_CREATE_3D` fields as
    /// userspace supplies them (`target`, `format`, `bind`, ...).
    fn create_resource_3d(&self, request: ResourceCreate3D) -> DriverResult;

    /// Transfers a box of a 3D resource from the guest to the host.
    fn transfer_to_host_3d(&self, request: Transfer3D) -> DriverResult;

    /// Transfers a box of a 3D resource from the host to the guest.
    fn transfer_from_host_3d(&self, request: Transfer3D) -> DriverResult;

    /// Submits a command stream and returns the fence the host will signal.
    fn submit(&self, ctx_id: u32, fence: GpuFenceId, cmds: &[u8]) -> DriverResult;

    /// Attaches `resource_id` to the 3D context `ctx_id`.
    ///
    /// The host renderer only resolves a resource named by a virgl command
    /// stream when that resource is attached to the submitting context
    /// (`virgl_renderer_ctx_attach_resource`), which Linux issues before the
    /// first submit that names it.
    fn attach_resource(&self, ctx_id: u32, resource_id: u32) -> DriverResult;

    /// Gives `resource_id` the guest memory window at `paddr` as its backing.
    ///
    /// This is `RESOURCE_ATTACH_BACKING`: a 3D resource is created empty and
    /// gains its contents through this call, the same way a scanout resource
    /// does inside [`DisplayDevice::create_scanout_resource`].
    ///
    /// # Safety
    ///
    /// `paddr..paddr + length_bytes` must stay live and unshared for as long
    /// as the resource exists.
    unsafe fn attach_backing(
        &self,
        resource_id: u32,
        paddr: u64,
        length_bytes: u32,
    ) -> DriverResult;

    /// Creates one blob resource over the guest memory window at `paddr`.
    ///
    /// This is `RESOURCE_CREATE_BLOB` with a single `mem_entry`: `blob_mem`
    /// and `blob_flags` are the userspace values, `blob_id` is the host blob
    /// a `HOST3D` caller references (zero for guest blobs). The caller owns
    /// the memory window and must keep it valid and unshared for as long as
    /// the resource exists.
    ///
    /// # Safety
    ///
    /// `paddr..paddr + length_bytes` must stay live and unshared for as long
    /// as the resource exists.
    #[expect(
        clippy::too_many_arguments,
        reason = "CG-024-lint-exceptions: RESOURCE_CREATE_BLOB is one flat eight-field wire \
                  request; splitting it would hide the ABI mapping"
    )]
    unsafe fn create_blob(
        &self,
        ctx_id: u32,
        resource_id: u32,
        blob_mem: u32,
        blob_flags: u32,
        size: u64,
        blob_id: u64,
        paddr: u64,
        length_bytes: u32,
    ) -> DriverResult;
}

/// One `RESOURCE_CREATE_3D` request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ResourceCreate3D {
    /// Context the resource belongs to.
    pub ctx_id: u32,
    /// Driver-local resource id.
    pub resource_id: u32,
    /// Texture target (`PIPE_TEXTURE_*`).
    pub target: u32,
    /// Pixel format (`PIPE_FORMAT_*`).
    pub format: u32,
    /// Bind flags (`VIRGL_BIND_*`).
    pub bind: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Depth.
    pub depth: u32,
    /// Array size.
    pub array_size: u32,
    /// Last mip level.
    pub last_level: u32,
    /// Sample count.
    pub nr_samples: u32,
    /// Resource flags.
    pub flags: u32,
}

/// One `TRANSFER_{TO,FROM}_HOST_3D` request.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Transfer3D {
    /// Context of the resource.
    pub ctx_id: u32,
    /// Driver-local resource id.
    pub resource_id: u32,
    /// The transferred box.
    pub x: u32,
    /// The transferred box.
    pub y: u32,
    /// The transferred box.
    pub z: u32,
    /// The transferred box.
    pub w: u32,
    /// The transferred box.
    pub h: u32,
    /// The transferred box.
    pub d: u32,
    /// Byte offset inside the mip level.
    pub offset: u64,
    /// Mip level.
    pub level: u32,
    /// Row stride.
    pub stride: u32,
    /// Layer stride.
    pub layer_stride: u32,
}

#[path = "../ut-tests/display_contract.rs"]
mod display_contract_tests;
