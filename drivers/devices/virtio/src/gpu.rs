// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO GPU driver adapter.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicU64, Ordering};

use display::{DisplayDevice, DisplayInfo, ScanoutRect, ScanoutResource};
use driver_base::{Device, DeviceKind, DriverError, DriverResult};
use kspin::SpinNoIrq;
use virtio_drivers::{
    Hal, PAGE_SIZE,
    config::{ReadOnly, WriteOnly, read_config, write_config},
    queue::VirtQueue,
    transport::Transport,
};
use zerocopy::{FromBytes, FromZeros, Immutable, IntoBytes, KnownLayout};

const QUEUE_SIZE: u16 = 2;
const QUEUE_TRANSMIT: u16 = 0;
const QUEUE_CURSOR: u16 = 1;
const SCANOUT_ID: u32 = 0;
/// Config-space event bit the host sets when the display configuration changed.
const EVENT_DISPLAY: u32 = 1 << 0;

#[derive(Default)]
struct ScanoutBinding {
    active: Option<(u32, Rect)>,
}

impl ScanoutBinding {
    fn needs_set_scanout(&self, resource_id: u32, rect: Rect) -> bool {
        self.active != Some((resource_id, rect))
    }

    fn publish(&mut self, resource_id: u32, rect: Rect) {
        self.active = Some((resource_id, rect));
    }

    fn clear_if_matches(&mut self, resource_id: u32) {
        if self
            .active
            .is_some_and(|(active_resource, _)| active_resource == resource_id)
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
    inner: SpinNoIrq<VirtIoGpu2d<H, T>>,
}

// SAFETY: VirtIoGpuDev serializes access to the inner VirtIoGpu2d through its own
// `SpinNoIrq` lock. The inner type is not auto Send/Sync due to PhantomData,
// but it is safe to transfer across threads and share behind that lock.
unsafe impl<H: Hal, T: Transport> Send for VirtIoGpuDev<H, T> {}
// SAFETY: shared references are synchronized by the same `SpinNoIrq` lock, so
// immutable aliasing across threads does not permit unsynchronized device access.
unsafe impl<H: Hal, T: Transport> Sync for VirtIoGpuDev<H, T> {}

impl<H: Hal, T: Transport> VirtIoGpuDev<H, T> {
    /// Creates a new driver instance and reads the display resolution.
    pub fn try_new(transport: T) -> DriverResult<Self> {
        let mut device = VirtIoGpu2d::new(transport).map_err(crate::as_driver_error)?;
        let (width, height) = device.resolution().map_err(crate::as_driver_error)?;

        Ok(Self {
            info: AtomicU64::new(pack_display_info(DisplayInfo { width, height })),
            inner: SpinNoIrq::new(device),
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

    fn refresh_info(&self) -> Option<DisplayInfo> {
        let mut inner = self.inner.lock();
        // The host reports a display change by setting the event bit in the
        // config space; there is no interrupt for it here, so the query paths
        // ask for it explicitly and acknowledge what they consumed.
        match inner.take_display_event() {
            Ok(true) => {}
            Ok(false) => return None,
            Err(err) => {
                log::warn!("virtio-gpu: cannot read the display event: {err:?}");
                return None;
            }
        }
        let (width, height) = match inner.resolution() {
            Ok(resolution) => resolution,
            Err(err) => {
                log::warn!("virtio-gpu: cannot read the display size: {err:?}");
                return None;
            }
        };

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

    fn need_flush(&self) -> bool {
        false
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }

    fn create_scanout_resource(
        &self,
        resource: ScanoutResource,
        paddr: u64,
        length: u32,
    ) -> DriverResult {
        if length == 0 || resource.pitch == 0 {
            return Err(DriverError::InvalidInput);
        }
        self.inner
            .lock()
            .create_resource(resource, paddr, length)
            .map_err(crate::as_driver_error)
    }

    fn destroy_scanout_resource(&self, resource_id: u32) -> DriverResult {
        self.inner
            .lock()
            .destroy_resource(resource_id)
            .map_err(crate::as_driver_error)
    }

    fn present_scanout_resource(&self, resource_id: u32, rect: ScanoutRect) -> DriverResult {
        self.inner
            .lock()
            .present_resource(resource_id, rect)
            .map_err(crate::as_driver_error)
    }

    fn flush_scanout_resource(&self, resource_id: u32, rect: ScanoutRect) -> DriverResult {
        self.inner
            .lock()
            .flush_resource(resource_id, rect)
            .map_err(crate::as_driver_error)
    }
}

// NOTE on protocol duplication: the command types below and the request/command
// methods above mirror the private internals of `virtio-drivers`
// (`kvirtiodrivers` 0.13.1, `device::gpu::VirtIOGpu`). The upstream methods we
// need (resource_create_2d / set_scanout / transfer_to_host_2d / resource_flush
// / attach/detach/unref) are private there, and the crate is a registry
// dependency this PR cannot extend, so the scanout subset is kept locally. It
// is deliberately trimmed: no EDID, cursor, or framebuffer-DMA paths, and a
// minimal QUEUE_SIZE of 2. This also documents the exact wire format we rely
// on instead of depending on upstream internals shifting.

struct VirtIoGpu2d<H: Hal, T: Transport> {
    transport: T,
    control_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    _cursor_queue: VirtQueue<H, { QUEUE_SIZE as usize }>,
    queue_buf_send: Box<[u8]>,
    queue_buf_recv: Box<[u8]>,
    /// Resource currently bound to scanout 0, protected by the GPU state lock.
    active_scanout: ScanoutBinding,
}

impl<H: Hal, T: Transport> VirtIoGpu2d<H, T> {
    fn new(mut transport: T) -> virtio_drivers::Result<Self> {
        let negotiated_features = transport.begin_init(SUPPORTED_FEATURES);

        let events_read = read_config!(transport, Config, events_read)?;
        let num_scanouts = read_config!(transport, Config, num_scanouts)?;
        log::info!(
            "virtio-gpu: events_read={:#x}, num_scanouts={:#x}",
            events_read,
            num_scanouts
        );

        let control_queue = VirtQueue::new(
            &mut transport,
            QUEUE_TRANSMIT,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
            negotiated_features.contains(Features::ACCESS_PLATFORM),
        )?;
        let cursor_queue = VirtQueue::new(
            &mut transport,
            QUEUE_CURSOR,
            negotiated_features.contains(Features::RING_INDIRECT_DESC),
            negotiated_features.contains(Features::RING_EVENT_IDX),
            negotiated_features.contains(Features::ACCESS_PLATFORM),
        )?;

        let queue_buf_send = FromZeros::new_box_zeroed_with_elems(PAGE_SIZE).unwrap();
        let queue_buf_recv = FromZeros::new_box_zeroed_with_elems(PAGE_SIZE).unwrap();

        transport.finish_init();

        Ok(Self {
            transport,
            control_queue,
            _cursor_queue: cursor_queue,
            queue_buf_send,
            queue_buf_recv,
            active_scanout: ScanoutBinding::default(),
        })
    }

    /// Reads and acknowledges the display-change event.
    ///
    /// Returns `true` when the host reported a display change that has not
    /// been acknowledged yet. Acknowledging is what stops the host from
    /// re-reporting the same change on the next call.
    fn take_display_event(&mut self) -> virtio_drivers::Result<bool> {
        let events = read_config!(self.transport, Config, events_read)?;
        if events & EVENT_DISPLAY == 0 {
            return Ok(false);
        }
        write_config!(self.transport, Config, events_clear, EVENT_DISPLAY)?;
        Ok(true)
    }

    fn resolution(&mut self) -> virtio_drivers::Result<(u32, u32)> {
        let display_info = self.get_display_info()?;
        if display_info.enabled == 0 {
            // Host disabled this scanout: rect is all-zero, which would
            // surface to DRM/fbdev as a bogus 0x0 mode.
            return Err(virtio_drivers::Error::IoError);
        }
        Ok((display_info.rect.width, display_info.rect.height))
    }

    fn create_resource(
        &mut self,
        resource: ScanoutResource,
        paddr: u64,
        length: u32,
    ) -> virtio_drivers::Result {
        if resource.format != display::ScanoutFormat::Bgra8888 {
            return Err(virtio_drivers::Error::InvalidParam);
        }
        self.resource_create_2d(resource.id, resource.width, resource.height)?;
        self.resource_attach_backing(resource.id, paddr, length)
    }

    fn destroy_resource(&mut self, resource_id: u32) -> virtio_drivers::Result {
        self.resource_detach_backing(resource_id)?;
        let result = self.resource_unref(resource_id);
        if result.is_ok() {
            self.active_scanout.clear_if_matches(resource_id);
        }
        result
    }

    fn flush_resource(&mut self, resource_id: u32, rect: ScanoutRect) -> virtio_drivers::Result {
        // Transfer the updated pixels and flush them. Deliberately no
        // SET_SCANOUT: a dirty flush must not change the scanout binding.
        let rect = Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        };
        self.transfer_to_host_2d(rect, 0, resource_id)?;
        self.resource_flush(rect, resource_id)
    }

    fn present_resource(&mut self, resource_id: u32, rect: ScanoutRect) -> virtio_drivers::Result {
        // Every present transfers the updated pixels and flushes the resource.
        // SET_SCANOUT is needed only when the active resource or scanout
        // rectangle changes. These commands remain synchronous, and transfer
        // must complete before a new scanout binding is installed.
        let rect = Rect {
            x: rect.x,
            y: rect.y,
            width: rect.width,
            height: rect.height,
        };
        let result = (|| {
            self.transfer_to_host_2d(rect, 0, resource_id)?;
            if self.active_scanout.needs_set_scanout(resource_id, rect) {
                self.set_scanout(rect, SCANOUT_ID, resource_id)?;
                self.active_scanout.publish(resource_id, rect);
            }
            self.resource_flush(rect, resource_id)
        })();
        if result.is_err() {
            self.active_scanout.clear();
        }
        result
    }

    fn request<Req: IntoBytes + Immutable, Rsp: FromBytes>(
        &mut self,
        req: Req,
    ) -> virtio_drivers::Result<Rsp> {
        // The send buffer is PAGE_SIZE while every request here is a fixed
        // ~32-byte struct, so the conversion cannot fail in practice; still,
        // propagate instead of panicking on malformed device-side data.
        req.write_to_prefix(&mut self.queue_buf_send)
            .map_err(|_| virtio_drivers::Error::IoError)?;
        self.control_queue.add_notify_wait_pop(
            &[&self.queue_buf_send],
            &mut [&mut self.queue_buf_recv],
            &mut self.transport,
        )?;
        Rsp::read_from_prefix(&self.queue_buf_recv)
            .map(|(rsp, _)| rsp)
            .map_err(|_| virtio_drivers::Error::IoError)
    }

    fn get_display_info(&mut self) -> virtio_drivers::Result<RespDisplayInfo> {
        let info: RespDisplayInfo =
            self.request(CtrlHeader::with_type(Command::GET_DISPLAY_INFO))?;
        info.header.check_type(Command::OK_DISPLAY_INFO)?;
        Ok(info)
    }

    fn resource_create_2d(
        &mut self,
        resource_id: u32,
        width: u32,
        height: u32,
    ) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceCreate2D {
            header: CtrlHeader::with_type(Command::RESOURCE_CREATE_2D),
            resource_id,
            format: Format::B8G8R8A8UNORM,
            width,
            height,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn set_scanout(
        &mut self,
        rect: Rect,
        scanout_id: u32,
        resource_id: u32,
    ) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(SetScanout {
            header: CtrlHeader::with_type(Command::SET_SCANOUT),
            rect,
            scanout_id,
            resource_id,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn resource_flush(&mut self, rect: Rect, resource_id: u32) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceFlush {
            header: CtrlHeader::with_type(Command::RESOURCE_FLUSH),
            rect,
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn transfer_to_host_2d(
        &mut self,
        rect: Rect,
        offset: u64,
        resource_id: u32,
    ) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(TransferToHost2D {
            header: CtrlHeader::with_type(Command::TRANSFER_TO_HOST_2D),
            rect,
            offset,
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn resource_attach_backing(
        &mut self,
        resource_id: u32,
        paddr: u64,
        length: u32,
    ) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceAttachBacking {
            header: CtrlHeader::with_type(Command::RESOURCE_ATTACH_BACKING),
            resource_id,
            nr_entries: 1,
            addr: paddr,
            length,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn resource_detach_backing(&mut self, resource_id: u32) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceDetachBacking {
            header: CtrlHeader::with_type(Command::RESOURCE_DETACH_BACKING),
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }

    fn resource_unref(&mut self, resource_id: u32) -> virtio_drivers::Result {
        let rsp: CtrlHeader = self.request(ResourceUnref {
            header: CtrlHeader::with_type(Command::RESOURCE_UNREF),
            resource_id,
            _padding: 0,
        })?;
        rsp.check_type(Command::OK_NODATA)
    }
}

impl<H: Hal, T: Transport> Drop for VirtIoGpu2d<H, T> {
    fn drop(&mut self) {
        self.transport.queue_unset(QUEUE_TRANSMIT);
        self.transport.queue_unset(QUEUE_CURSOR);
    }
}

#[repr(C)]
struct Config {
    events_read: ReadOnly<u32>,
    events_clear: WriteOnly<u32>,
    num_scanouts: ReadOnly<u32>,
}

bitflags::bitflags! {
    #[derive(Copy, Clone, Debug, Default, Eq, PartialEq)]
    struct Features: u64 {
        const VIRGL                 = 1 << 0;
        const EDID                  = 1 << 1;
        const NOTIFY_ON_EMPTY       = 1 << 24;
        const ANY_LAYOUT            = 1 << 27;
        const RING_INDIRECT_DESC    = 1 << 28;
        const RING_EVENT_IDX        = 1 << 29;
        const UNUSED                = 1 << 30;
        const VERSION_1             = 1 << 32;
        const ACCESS_PLATFORM       = 1 << 33;
        const RING_PACKED           = 1 << 34;
        const IN_ORDER              = 1 << 35;
        const ORDER_PLATFORM        = 1 << 36;
        const SR_IOV                = 1 << 37;
        const NOTIFICATION_DATA     = 1 << 38;
    }
}

const SUPPORTED_FEATURES: Features = Features::RING_EVENT_IDX
    .union(Features::RING_INDIRECT_DESC)
    .union(Features::VERSION_1);

#[repr(transparent)]
#[derive(Clone, Copy, Debug, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq)]
struct Command(u32);

impl Command {
    const GET_DISPLAY_INFO: Command = Command(0x100);
    const OK_DISPLAY_INFO: Command = Command(0x1101);
    const OK_NODATA: Command = Command(0x1100);
    const RESOURCE_ATTACH_BACKING: Command = Command(0x106);
    const RESOURCE_CREATE_2D: Command = Command(0x101);
    const RESOURCE_DETACH_BACKING: Command = Command(0x107);
    const RESOURCE_FLUSH: Command = Command(0x104);
    const RESOURCE_UNREF: Command = Command(0x102);
    const SET_SCANOUT: Command = Command(0x103);
    const TRANSFER_TO_HOST_2D: Command = Command(0x105);
}

#[repr(C)]
#[derive(Debug, Clone, Copy, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct CtrlHeader {
    hdr_type: Command,
    flags: u32,
    fence_id: u64,
    ctx_id: u32,
    _padding: u32,
}

impl CtrlHeader {
    fn with_type(hdr_type: Command) -> CtrlHeader {
        CtrlHeader {
            hdr_type,
            flags: 0,
            fence_id: 0,
            ctx_id: 0,
            _padding: 0,
        }
    }

    fn check_type(&self, expected: Command) -> virtio_drivers::Result {
        if self.hdr_type == expected {
            Ok(())
        } else {
            Err(virtio_drivers::Error::IoError)
        }
    }
}

#[repr(C)]
#[derive(
    Debug, Copy, Clone, Default, Eq, FromBytes, Immutable, IntoBytes, KnownLayout, PartialEq,
)]
struct Rect {
    x: u32,
    y: u32,
    width: u32,
    height: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, Immutable, KnownLayout)]
struct RespDisplayInfo {
    header: CtrlHeader,
    rect: Rect,
    enabled: u32,
    flags: u32,
}

#[repr(C)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
struct ResourceCreate2D {
    header: CtrlHeader,
    resource_id: u32,
    format: Format,
    width: u32,
    height: u32,
}

#[repr(u32)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
enum Format {
    #[expect(
        clippy::upper_case_acronyms,
        reason = "CG-032-camel-case-types: pixel format name mirrors the virtio GPU hardware \
                  format B8G8R8A8_UNORM"
    )]
    B8G8R8A8UNORM = 1,
}

#[repr(C)]
#[derive(Debug, Immutable, IntoBytes, KnownLayout)]
struct ResourceAttachBacking {
    header: CtrlHeader,
    resource_id: u32,
    nr_entries: u32,
    addr: u64,
    length: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct ResourceDetachBacking {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct ResourceUnref {
    header: CtrlHeader,
    resource_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct SetScanout {
    header: CtrlHeader,
    rect: Rect,
    scanout_id: u32,
    resource_id: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct TransferToHost2D {
    header: CtrlHeader,
    rect: Rect,
    offset: u64,
    resource_id: u32,
    _padding: u32,
}

#[repr(C)]
#[derive(Debug, FromBytes, Immutable, IntoBytes, KnownLayout)]
struct ResourceFlush {
    header: CtrlHeader,
    rect: Rect,
    resource_id: u32,
    _padding: u32,
}

unittest::path_mod_test!(gpu_unittest, "../ut-tests/gpu_unittest.rs");
