// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO GPU driver adapter.

use core::sync::atomic::{AtomicU64, Ordering};

use display::{
    CapsetInfo, DisplayDevice, DisplayInfo, DisplayIrqStatus, DriverInfo, GpuCommand, GpuFenceId,
    ResourceCreate3D, ScanoutRect, ScanoutResource, Transfer3D,
};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal,
    device::gpu::{GpuBox, Rect, VirtIOGpu},
    transport::{InterruptStatus, Transport},
};

/// The scanout this adapter presents to.
const SCANOUT_ID: u32 = 0;

/// The Linux-compatible identity of this driver.
const DRIVER_NAME: &str = "virtio_gpu";
const DRIVER_DATE: &str = "2026-06-02";
const DRIVER_DESC: &str = "X-Kernel simple DRM driver";

/// The resource and rectangle the host was last asked to scan out.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
struct Binding {
    resource: u32,
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

impl Binding {
    const fn new(resource_id: u32, rect: Rect) -> Self {
        Self {
            resource: resource_id,
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        }
    }
}

#[derive(Default)]
struct ScanoutBinding {
    active: Option<Binding>,
}

impl ScanoutBinding {
    fn needs_set_scanout(&self, resource_id: u32, rect: Rect) -> bool {
        self.active != Some(Binding::new(resource_id, rect))
    }

    fn publish(&mut self, resource_id: u32, rect: Rect) {
        self.active = Some(Binding::new(resource_id, rect));
    }

    fn clear_if_matches(&mut self, resource_id: u32) {
        if self
            .active
            .is_some_and(|active| active.resource == resource_id)
        {
            self.active = None;
        }
    }

    fn clear(&mut self) {
        self.active = None;
    }
}

/// Packs a visible size into the one atomic word the driver keeps.
const fn pack_display_info(info: DisplayInfo) -> u64 {
    ((info.width as u64) << 32) | info.height as u64
}

/// Unpacks the atomic word written by [`pack_display_info`].
const fn unpack_display_info(packed: u64) -> DisplayInfo {
    DisplayInfo {
        width: (packed >> 32) as u32,
        height: packed as u32,
    }
}

/// The VirtIO GPU device driver.
pub struct VirtIoGpuDev<H: Hal, T: Transport> {
    /// Host-reported visible size, packed as `width << 32 | height`.
    ///
    /// Only the query path (`info`/`refresh_info`) touches it and no interrupt
    /// or softirq context does, so an atomic copy needs no lock. A plain
    /// relaxed load/store is enough: the value publishes no other state, and a
    /// reader that observes a stale size is corrected by the next refresh.
    info: AtomicU64,
    inner: SpinNoIrq<VirtIoGpuInner<H, T>>,
}

/// The vendor device plus the adapter state that is not part of the device.
struct VirtIoGpuInner<H: Hal, T: Transport> {
    gpu: VirtIOGpu<H, T>,
    /// The resource the host was last asked to scan out.
    scanout: ScanoutBinding,
    /// Whether the 3D feature was negotiated.
    has_virgl: bool,
}

// SAFETY: VirtIoGpuDev serializes access to the inner device through its own
// `SpinNoIrq` lock, so the device is only ever touched by one context at a
// time. `VirtIOGpu` is not `Send`/`Sync` because it owns raw queue memory.
unsafe impl<H: Hal, T: Transport> Send for VirtIoGpuDev<H, T> {}
// SAFETY: shared references are synchronized by the same `SpinNoIrq` lock, so
// immutable aliasing across threads cannot reach the device concurrently.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoGpuDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
    /// Creates a new driver instance and reads the display resolution.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let gpu = VirtIOGpu::new(transport).map_err(crate::as_driver_error)?;
        let has_virgl = gpu.has_virgl();
        let mut inner = VirtIoGpuInner {
            gpu,
            scanout: ScanoutBinding::default(),
            has_virgl,
        };
        let (width, height) = inner.gpu.resolution().map_err(crate::as_driver_error)?;
        if inner.has_virgl {
            log::info!("virtio-gpu: 3D (virgl) feature negotiated");
        }

        Ok(Self {
            info: AtomicU64::new(pack_display_info(DisplayInfo { width, height })),
            inner: SpinNoIrq::new(inner),
        })
    }
}

impl<H: Hal, T: Transport> Device for VirtIoGpuDev<H, T> {
    fn name(&self) -> &str {
        "virtio-gpu"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Display
    }
}

impl<H: Hal, T: Transport> DisplayDevice for VirtIoGpuDev<H, T> {
    fn info(&self) -> DisplayInfo {
        unpack_display_info(self.info.load(Ordering::Relaxed))
    }

    fn driver_info(&self) -> DriverInfo {
        DriverInfo {
            name: DRIVER_NAME,
            date: DRIVER_DATE,
            desc: DRIVER_DESC,
        }
    }

    fn refresh_info(&self) -> Option<DisplayInfo> {
        let mut inner = self.inner.lock();
        // The host reports a display change by setting the event bit in the
        // config space. Reading it does not acknowledge it: the device keeps
        // the bit set until the driver writes `events_clear`, so every query
        // would otherwise observe the same stale change forever.
        if !inner.gpu.display_changed() {
            return None;
        }
        let (width, height) = match inner.gpu.resolution() {
            Ok(resolution) => resolution,
            Err(err) => {
                log::warn!("virtio-gpu: cannot read the display size: {err:?}");
                return None;
            }
        };
        if !inner.gpu.ack_display_event() {
            log::warn!("virtio-gpu: cannot acknowledge the display event");
        }

        // The device lock above serializes refreshes, so the compare and the
        // store cannot race another refresh; the relaxed access only has to
        // carry the value, not order it against other state.
        let previous = unpack_display_info(self.info.load(Ordering::Relaxed));
        let refreshed = DisplayInfo { width, height };
        if previous == refreshed {
            return None;
        }
        log::info!(
            "virtio-gpu: display size changed from {}x{} to {}x{}",
            previous.width,
            previous.height,
            refreshed.width,
            refreshed.height
        );
        self.info
            .store(pack_display_info(refreshed), Ordering::Relaxed);
        Some(refreshed)
    }

    fn ack_irq(&self) -> DisplayIrqStatus {
        let mut inner = self.inner.lock();
        let status = inner.gpu.ack_interrupt();
        DisplayIrqStatus {
            queue: status.contains(InterruptStatus::QUEUE_INTERRUPT),
            config: status.contains(InterruptStatus::DEVICE_CONFIGURATION_INTERRUPT),
        }
    }

    fn need_flush(&self) -> bool {
        false
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }

    fn create_scanout_resource(
        &self,
        resource: ScanoutResource,
        window: &display::ScanoutWindow<'_>,
    ) -> DriverResult {
        if resource.pitch == 0 {
            return Err(DriverError::InvalidInput);
        }
        if resource.format != display::ScanoutFormat::Bgra8888 {
            return Err(DriverError::InvalidInput);
        }
        let mut inner = self.inner.lock();
        inner
            .gpu
            .resource_create_2d(resource.id, resource.width, resource.height)
            .map_err(crate::as_driver_error)?;
        let range = window.range();
        let length = u32::try_from(range.end.as_usize() - range.start.as_usize())
            .map_err(|_| DriverError::InvalidInput)?;
        inner
            .gpu
            .resource_attach_backing(resource.id, range.start.as_usize() as u64, length)
            .map_err(crate::as_driver_error)
    }

    fn destroy_scanout_resource(&self, resource_id: u32) -> DriverResult {
        let mut inner = self.inner.lock();
        inner
            .gpu
            .resource_detach_backing(resource_id)
            .map_err(crate::as_driver_error)?;
        let result = inner.gpu.resource_unref(resource_id);
        if result.is_ok() {
            inner.scanout.clear_if_matches(resource_id);
        }
        result.map_err(crate::as_driver_error)
    }

    fn present_scanout_resource(
        &self,
        resource_id: u32,
        rect: ScanoutRect,
        host_rendered: bool,
    ) -> DriverResult {
        // Every present flushes the resource; a guest-rendered resource also
        // transfers its updated pixels first. SET_SCANOUT is needed only when
        // the active resource or scanout rectangle changes. These commands
        // remain synchronous, and transfer must complete before a new scanout
        // binding is installed.
        //
        // A host-rendered (3D/virgl) resource has no guest pixels to push:
        // transferring its pages would overwrite the texture the host just
        // rendered into, which is exactly how a compositor's output went
        // black. Linux presents such a resource without a 2D transfer.
        let rect = Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        };
        let mut inner = self.inner.lock();
        let result = (|| {
            if !host_rendered {
                inner
                    .gpu
                    .transfer_to_host_2d(rect, 0, resource_id)
                    .map_err(crate::as_driver_error)?;
            }
            if inner.scanout.needs_set_scanout(resource_id, rect) {
                inner
                    .gpu
                    .set_scanout(rect, SCANOUT_ID, resource_id)
                    .map_err(crate::as_driver_error)?;
                inner.scanout.publish(resource_id, rect);
            }
            inner
                .gpu
                .resource_flush(rect, resource_id)
                .map_err(crate::as_driver_error)
        })();
        if result.is_err() {
            inner.scanout.clear();
        }
        result
    }

    fn flush_scanout_resource(
        &self,
        resource_id: u32,
        rect: ScanoutRect,
        host_rendered: bool,
    ) -> DriverResult {
        // Transfer the updated pixels and flush them. Deliberately no
        // SET_SCANOUT: a dirty flush must not change the scanout binding.
        // A host-rendered resource is already current on the host, so only the
        // flush is needed.
        let rect = Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        };
        let mut inner = self.inner.lock();
        if !host_rendered {
            inner
                .gpu
                .transfer_to_host_2d(rect, 0, resource_id)
                .map_err(crate::as_driver_error)?;
        }
        inner
            .gpu
            .resource_flush(rect, resource_id)
            .map_err(crate::as_driver_error)
    }

    fn gpu(&self) -> Option<&dyn GpuCommand> {
        self.has_virgl().then_some(self as &dyn GpuCommand)
    }

    fn has_resource_blob(&self) -> bool {
        self.inner.lock().gpu.has_resource_blob()
    }

    fn has_context_init(&self) -> bool {
        self.inner.lock().gpu.has_context_init()
    }

    fn edid(&self) -> Option<::alloc::vec::Vec<u8>> {
        let mut inner = self.inner.lock();
        match inner.gpu.get_edid(SCANOUT_ID) {
            Ok(edid) => Some(edid.bytes().to_vec()),
            // A host that did not negotiate EDID, or has no descriptor for
            // this scanout, has nothing to publish.
            Err(_) => None,
        }
    }

    fn is_connected(&self) -> bool {
        let info = self.info();
        info.width != 0 && info.height != 0
    }
}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
    /// Whether the VIRGL feature was negotiated.
    fn has_virgl(&self) -> bool {
        self.inner.lock().has_virgl
    }
}

impl<H: Hal, T: Transport> GpuCommand for VirtIoGpuDev<H, T> {
    fn is_available(&self) -> bool {
        self.has_virgl()
    }

    fn capset_info(&self, index: u32) -> DriverResult<Option<CapsetInfo>> {
        let mut inner = self.inner.lock();
        match inner.gpu.get_capset_info(index) {
            Ok(info) => Ok(Some(CapsetInfo {
                id: info.capset_id,
                max_version: info.capset_max_version,
                max_size: info.capset_max_size,
            })),
            // The host answers past the last capset with an error; that is
            // the end of the list, not a fault.
            Err(_) => Ok(None),
        }
    }

    fn capset(&self, id: u32, version: u32, size: u32) -> DriverResult<alloc::vec::Vec<u8>> {
        let mut inner = self.inner.lock();
        inner
            .gpu
            .get_capset(id, version, size)
            .map_err(crate::as_driver_error)
    }

    fn create_context(&self, ctx_id: u32, name: &str, context_init: u32) -> DriverResult {
        let mut inner = self.inner.lock();
        inner
            .gpu
            .ctx_create(ctx_id, name, context_init)
            .map_err(crate::as_driver_error)
    }

    fn destroy_context(&self, ctx_id: u32) -> DriverResult {
        let mut inner = self.inner.lock();
        inner
            .gpu
            .ctx_destroy(ctx_id)
            .map_err(crate::as_driver_error)
    }

    fn create_resource_3d(&self, request: ResourceCreate3D) -> DriverResult {
        let mut inner = self.inner.lock();
        inner
            .gpu
            .resource_create_3d(
                request.ctx_id,
                request.resource_id,
                request.target,
                request.format,
                request.bind,
                request.width,
                request.height,
                request.depth,
                request.array_size,
                request.last_level,
                request.nr_samples,
                request.flags,
            )
            .map_err(crate::as_driver_error)
    }

    fn transfer_to_host_3d(&self, request: Transfer3D) -> DriverResult {
        let mut inner = self.inner.lock();
        inner
            .gpu
            .transfer_to_host_3d(
                request.ctx_id,
                request.resource_id,
                gpu_box(&request),
                request.offset,
                request.level,
                request.stride,
                request.layer_stride,
            )
            .map_err(crate::as_driver_error)
    }

    fn transfer_from_host_3d(&self, request: Transfer3D) -> DriverResult {
        let mut inner = self.inner.lock();
        inner
            .gpu
            .transfer_from_host_3d(
                request.ctx_id,
                request.resource_id,
                gpu_box(&request),
                request.offset,
                request.level,
                request.stride,
                request.layer_stride,
            )
            .map_err(crate::as_driver_error)
    }

    fn submit(&self, ctx_id: u32, fence: GpuFenceId, cmds: &[u8]) -> DriverResult {
        let mut inner = self.inner.lock();
        let result = inner
            .gpu
            .submit_3d(ctx_id, fence, cmds)
            .map_err(crate::as_driver_error);
        // The host may have completed the stream before it answered; the
        // completion that matters is the one the fence reports, which the
        // caller observes through its own descriptor.
        result
    }

    fn attach_resource(&self, ctx_id: u32, resource_id: u32) -> DriverResult {
        let mut inner = self.inner.lock();
        inner
            .gpu
            .ctx_attach_resource(ctx_id, resource_id)
            .map_err(crate::as_driver_error)
    }

    unsafe fn attach_backing(
        &self,
        resource_id: u32,
        paddr: u64,
        length_bytes: u32,
    ) -> DriverResult {
        let mut inner = self.inner.lock();
        inner
            .gpu
            .resource_attach_backing(resource_id, paddr, length_bytes)
            .map_err(crate::as_driver_error)
    }

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
    ) -> DriverResult {
        let mut inner = self.inner.lock();
        // SAFETY: the caller owns the window and hands it to a GEM object that
        // keeps the pages allocated and unshared; the object's destructor
        // unrefs the resource before the pages are freed, which is exactly the
        // lifetime `resource_create_blob` demands of the entry list.
        unsafe {
            inner
                .gpu
                .resource_create_blob(
                    ctx_id,
                    resource_id,
                    blob_mem,
                    blob_flags,
                    size,
                    blob_id,
                    &[(paddr, length_bytes as usize)],
                )
                .map_err(crate::as_driver_error)
        }
    }
}

/// The box of one 3D transfer as the device describes it.
const fn gpu_box(request: &Transfer3D) -> GpuBox {
    GpuBox {
        x: request.x,
        y: request.y,
        z: request.z,
        w: request.w,
        h: request.h,
        d: request.d,
    }
}
