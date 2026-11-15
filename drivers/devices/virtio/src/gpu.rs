// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO GPU driver adapter.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use display::{
    CapsetInfo, DisplayDevice, DisplayInfo, DisplayIrqStatus, DriverInfo, GpuCommand, GpuFenceId,
    ResourceCreate3D, ScanoutRect, ScanoutResource, Transfer3D,
};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal,
    device::gpu::{GpuBox, GpuChannel, Rect, VirtIOGpu},
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

/// Task-context serialization and IRQ-safe notification supplied by the host.
pub trait GpuHost: Send + Sync {
    /// A sleepable lock for complete, potentially multi-command GPU operations.
    type Mutex: lock_api::RawMutex + Send + Sync;
    /// Whether the caller can take the operation mutex and park for an IRQ.
    fn can_sleep(&self) -> bool;
    /// Park the task until the predicate is true, including a wake before parking.
    fn wait_until(&self, ready: impl FnMut() -> bool);
    /// Wake waiters after the device reports a used buffer. Called from IRQ.
    fn notify(&self);
}

struct Registers<T, W> {
    transport: SpinNoIrq<Option<T>>,
    host: W,
}

struct IrqChannel<T, W>(Arc<Registers<T, W>>);

impl<T: Transport, W: GpuHost> GpuChannel for IrqChannel<T, W> {
    type Transport = T;

    fn with_transport<R>(&mut self, access: impl FnOnce(&mut T) -> R) -> R {
        let mut transport = self.0.transport.lock();
        #[expect(
            clippy::expect_used,
            reason = "the queue owner only accesses registers while the channel is live"
        )]
        access(transport.as_mut().expect("live GPU channel"))
    }

    fn shutdown(&mut self) {
        // No new commands can enter. Detach the IRQ view first; the device
        // reset may wait for acknowledgement and must run with IRQs enabled.
        let transport = self.0.transport.lock().take();
        if let Some(mut transport) = transport {
            transport.reset();
        }
    }

    fn wait_until(&self, ready: impl FnMut() -> bool) {
        self.0.host.wait_until(ready);
    }
}

/// VirtIO GPU command queues owned by the submitting task. The hard IRQ
/// acknowledges the transport and wakes that task without acquiring its mutex.
pub struct VirtIoGpuDev<H: Hal, T: Transport, W: GpuHost>(Arc<GpuState<H, T, W>>);

struct GpuState<H: Hal, T: Transport, W: GpuHost> {
    // Width and height form one snapshot. It publishes no other state, so
    // readers and writers only need relaxed atomic access.
    info: AtomicU64,
    registers: Arc<Registers<T, W>>,
    inner: lock_api::Mutex<W::Mutex, Option<VirtIoGpuInner<H, T, W>>>,
}

impl<H: Hal, T: Transport, W: GpuHost> Clone for VirtIoGpuDev<H, T, W> {
    fn clone(&self) -> Self {
        Self(self.0.clone())
    }
}

/// The vendor device plus the adapter state that is not part of the device.
struct VirtIoGpuInner<H: Hal, T: Transport, W: GpuHost> {
    gpu: VirtIOGpu<H, IrqChannel<T, W>>,
    /// The resource the host was last asked to scan out.
    scanout: ScanoutBinding,
    /// Whether the 3D feature was negotiated.
    has_virgl: bool,
}

type Operation<'a, H, T, W> =
    lock_api::MappedMutexGuard<'a, <W as GpuHost>::Mutex, VirtIoGpuInner<H, T, W>>;

impl<H: Hal, T: Transport + Send, W: GpuHost> VirtIoGpuDev<H, T, W> {
    /// Prepare both queues without issuing a command or enabling the device.
    pub fn prepare(transport: T, host: W) -> DriverResult<Self> {
        let gpu = VirtIOGpu::prepare(transport, |t| {
            IrqChannel(Arc::new(Registers {
                transport: SpinNoIrq::new(Some(t)),
                host,
            }))
        })
        .map_err(crate::as_driver_error)?;
        let registers = gpu.channel().0.clone();
        let has_virgl = gpu.has_virgl();
        Ok(Self(Arc::new(GpuState {
            info: AtomicU64::new(0),
            registers,
            inner: lock_api::Mutex::new(Some(VirtIoGpuInner {
                gpu,
                scanout: ScanoutBinding::default(),
                has_virgl,
            })),
        })))
    }

    fn operation(&self) -> DriverResult<Operation<'_, H, T, W>> {
        if !self.0.registers.host.can_sleep() {
            return Err(DriverError::WouldBlock);
        }
        lock_api::MutexGuard::try_map(self.0.inner.lock(), Option::as_mut)
            .map_err(|_| DriverError::BadState)
    }

    /// Set DRIVER_OK and discover the display with IRQ delivery already active.
    pub fn start(&self) -> DriverResult<()> {
        let mut inner = self.operation()?;
        inner.gpu.enable();
        let (width, height) = inner.gpu.resolution().map_err(crate::as_driver_error)?;
        self.0.info.store(
            pack_display_info(DisplayInfo { width, height }),
            Ordering::Relaxed,
        );
        Ok(())
    }

    /// Drain the active operation while its IRQ is still registered, then
    /// release the queues and reject new requests through surviving handles.
    pub fn close(&self) {
        let inner = self.0.inner.lock().take();
        // Surviving handles can now observe closure while reset acknowledges.
        drop(inner);
    }
}

impl<H: Hal, T: Transport + Send, W: GpuHost> Device for VirtIoGpuDev<H, T, W> {
    fn name(&self) -> &str {
        "virtio-gpu"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Display
    }
}

impl<H: Hal, T: Transport + Send, W: GpuHost> DisplayDevice for VirtIoGpuDev<H, T, W> {
    fn info(&self) -> DisplayInfo {
        unpack_display_info(self.0.info.load(Ordering::Relaxed))
    }

    fn driver_info(&self) -> DriverInfo {
        DriverInfo {
            name: DRIVER_NAME,
            date: DRIVER_DATE,
            desc: DRIVER_DESC,
        }
    }

    fn refresh_info(&self) -> Option<DisplayInfo> {
        let mut inner = self.operation().ok()?;
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
        let previous = unpack_display_info(self.0.info.load(Ordering::Relaxed));
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
        self.0
            .info
            .store(pack_display_info(refreshed), Ordering::Relaxed);
        Some(refreshed)
    }

    fn ack_irq(&self) -> DisplayIrqStatus {
        let status = self
            .0
            .registers
            .transport
            .lock()
            .as_mut()
            .map_or(InterruptStatus::empty(), Transport::ack_interrupt);
        if status.contains(InterruptStatus::QUEUE_INTERRUPT) {
            self.0.registers.host.notify();
        }
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
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
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
        self.operation()
            .is_ok_and(|inner| inner.gpu.has_resource_blob())
    }

    fn has_context_init(&self) -> bool {
        self.operation()
            .is_ok_and(|inner| inner.gpu.has_context_init())
    }

    fn edid(&self) -> Option<::alloc::vec::Vec<u8>> {
        let mut inner = self.operation().ok()?;
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

impl<H: Hal, T: Transport + Send, W: GpuHost> VirtIoGpuDev<H, T, W> {
    /// Whether the VIRGL feature was negotiated.
    fn has_virgl(&self) -> bool {
        self.operation().is_ok_and(|inner| inner.has_virgl)
    }
}

impl<H: Hal, T: Transport + Send, W: GpuHost> GpuCommand for VirtIoGpuDev<H, T, W> {
    fn is_available(&self) -> bool {
        self.has_virgl()
    }

    fn capset_info(&self, index: u32) -> DriverResult<Option<CapsetInfo>> {
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
        inner
            .gpu
            .get_capset(id, version, size)
            .map_err(crate::as_driver_error)
    }

    fn create_context(&self, ctx_id: u32, name: &str, context_init: u32) -> DriverResult {
        let mut inner = self.operation()?;
        inner
            .gpu
            .ctx_create(ctx_id, name, context_init)
            .map_err(crate::as_driver_error)
    }

    fn destroy_context(&self, ctx_id: u32) -> DriverResult {
        let mut inner = self.operation()?;
        inner
            .gpu
            .ctx_destroy(ctx_id)
            .map_err(crate::as_driver_error)
    }

    fn create_resource_3d(&self, request: ResourceCreate3D) -> DriverResult {
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
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
        let mut inner = self.operation()?;
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
