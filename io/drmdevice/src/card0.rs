// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `/dev/dri/card0` — minimal DRM character device.
//!
//! Single-CRTC, single-connector, single-plane simpledrm-class driver
//! over the existing `fbdevice` framebuffer. Covers legacy libdrm
//! (`CREATE_DUMB → ADDFB2 → SETCRTC → PAGE_FLIP`) and the atomic-KMS
//! path (`MODE_ATOMIC` + blob properties) used by modern compositors.

use alloc::{
    collections::{BTreeMap, VecDeque},
    format,
    string::String,
    sync::Arc,
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use bytemuck::bytes_of;
use kalloc::GlobalPage;
use khal::mem::v2p;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use kvfs::{DeviceFileOps, MmapMapper, NodeFlags, VfsError, VfsFile, VfsResult};
use memaddr::{PAGE_SIZE_4K, PhysAddrRange};
use posix_types::{UserPtr, UserRead, UserWrite};

use super::{consts::*, drm::*};

/// DRM ioctl handler trait. `const CMD` is injected by the `#[drm_ioctl]` attribute macro.
trait DrmIoctl: Sized {
    const CMD: u32;
    fn handle(dev: &Card0, arg: &mut Self) -> VfsResult<usize>;

    fn handle_raw(dev: &Card0, arg: UserPtr<Self>) -> VfsResult<usize>
    where
        Self: UserRead + UserWrite,
    {
        let mut ptr: Self = arg.read_vm().map_err(|_| kvfs::VfsError::BadAddress)?;
        let result = Self::handle(dev, &mut ptr);
        arg.write_vm(ptr).map_err(|_| kvfs::VfsError::BadAddress)?;
        result
    }
}

macro_rules! route_ioctls {
    ($cmd:expr, $arg:expr, $dev:expr; [ $($ty:ty),* $(,)? ]) => {
        match $cmd {
            $(
                <$ty as DrmIoctl>::CMD => {
                    let user_ptr = posix_types::UserPtr::<$ty>::from($arg);
                    <$ty as DrmIoctl>::handle_raw($dev, user_ptr)
                }
            )*
            _ => Err(kvfs::VfsError::OperationNotSupported),
        }
    };
}

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
struct DrmAuthMagic(DrmAuth);

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
struct DrmPrimeFdToHandle(DrmPrimeHandle);

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
struct DrmModeSetCrtc(DrmModeCrtc);

#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
struct DrmModeRmFb(u32);

impl DrmIoctl for DrmVersion {
    const CMD: u32 = iowr::<DrmVersion>(DRM_TYPE, 0x00);

    fn handle(_dev: &Card0, version: &mut Self) -> VfsResult<usize> {
        version.version_major = DRIVER_VERSION_MAJOR;
        version.version_minor = DRIVER_VERSION_MINOR;
        version.version_patchlevel = DRIVER_VERSION_PATCHLEVEL;
        version.name_len = DRIVER_NAME.len();
        version
            .name
            .write_vm_slice(DRIVER_NAME.as_bytes())
            .map_err(|_| VfsError::BadAddress)?;
        version.date_len = DRIVER_DATE.len();
        version
            .date
            .write_vm_slice(DRIVER_DATE.as_bytes())
            .map_err(|_| VfsError::BadAddress)?;
        version.desc_len = DRIVER_DESC.len();
        version
            .desc
            .write_vm_slice(DRIVER_DESC.as_bytes())
            .map_err(|_| VfsError::BadAddress)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmSetVersion {
    const CMD: u32 = iowr::<DrmSetVersion>(DRM_TYPE, 0x07);

    fn handle(_dev: &Card0, set_version: &mut Self) -> VfsResult<usize> {
        if set_version.drm_di_major < 0 {
            set_version.drm_di_major = 1;
        }
        if set_version.drm_di_minor < 0 {
            set_version.drm_di_minor = 4;
        }
        set_version.drm_dd_major = DRIVER_VERSION_MAJOR;
        set_version.drm_dd_minor = DRIVER_VERSION_MINOR;
        Ok(0)
    }
}

impl DrmIoctl for DrmUnique {
    const CMD: u32 = iowr::<DrmUnique>(DRM_TYPE, 0x01);

    fn handle(_dev: &Card0, unique: &mut Self) -> VfsResult<usize> {
        let unique_str: String = format!("{}:0", DRIVER_NAME);
        unique.unique_len = unique_str.len();
        unique
            .unique
            .write_vm_slice(unique_str.as_bytes())
            .map_err(|_| VfsError::BadAddress)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmGetCap {
    const CMD: u32 = iowr::<DrmGetCap>(DRM_TYPE, 0x0c);

    fn handle(_dev: &Card0, cap: &mut Self) -> VfsResult<usize> {
        cap.value = match cap.capability {
            DRM_CAP_DUMB_BUFFER => 1,
            DRM_CAP_TIMESTAMP_MONOTONIC => 1,
            DRM_CAP_CRTC_IN_VBLANK_EVENT => 1,
            DRM_CAP_ADDFB2_MODIFIERS => 1,
            _ => 0,
        };
        Ok(0)
    }
}

impl DrmIoctl for DrmSetClientCap {
    const CMD: u32 = iow::<DrmSetClientCap>(DRM_TYPE, 0x0d);

    fn handle(_dev: &Card0, _cap: &mut Self) -> VfsResult<usize> {
        Ok(0)
    }
}

impl DrmIoctl for DrmAuth {
    const CMD: u32 = iowr::<DrmAuth>(DRM_TYPE, 0x02);

    fn handle(_dev: &Card0, auth: &mut Self) -> VfsResult<usize> {
        auth.magic = 1;
        Ok(0)
    }
}

impl DrmIoctl for DrmAuthMagic {
    const CMD: u32 = iowr::<DrmAuth>(DRM_TYPE, 0x03);

    fn handle(_dev: &Card0, _auth: &mut Self) -> VfsResult<usize> {
        Ok(0)
    }
}

impl DrmIoctl for DrmModeDirtyFB {
    const CMD: u32 = iowr::<DrmModeDirtyFB>(DRM_TYPE, 0xB1);

    fn handle(_dev: &Card0, _dirty: &mut Self) -> VfsResult<usize> {
        Ok(0)
    }
}

impl DrmIoctl for DrmPrimeHandle {
    const CMD: u32 = iowr::<DrmPrimeHandle>(DRM_TYPE, 0x2d);

    fn handle(_dev: &Card0, prime: &mut Self) -> VfsResult<usize> {
        prime.fd = prime.handle as i32;
        Ok(0)
    }
}

impl DrmIoctl for DrmPrimeFdToHandle {
    const CMD: u32 = iowr::<DrmPrimeHandle>(DRM_TYPE, 0x2e);

    fn handle(_dev: &Card0, prime: &mut Self) -> VfsResult<usize> {
        prime.0.handle = prime.0.fd as u32;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCardRes {
    const CMD: u32 = iowr::<DrmModeCardRes>(DRM_TYPE, 0xA0);

    fn handle(_dev: &Card0, r: &mut Self) -> VfsResult<usize> {
        let (w, h) = display_resolution();
        r.min_width = w;
        r.max_width = w;
        r.min_height = h;
        r.max_height = h;

        r.count_fbs = 0;
        r.count_crtcs = report_user_array(r.crtc_id_ptr, r.count_crtcs, &[CRTC_ID])?;
        r.count_encoders = report_user_array(r.encoder_id_ptr, r.count_encoders, &[ENCODER_ID])?;
        r.count_connectors =
            report_user_array(r.connector_id_ptr, r.count_connectors, &[CONNECTOR_ID])?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCrtc {
    const CMD: u32 = iowr::<DrmModeCrtc>(DRM_TYPE, 0xA1);

    fn handle(dev: &Card0, crtc: &mut Self) -> VfsResult<usize> {
        if crtc.crtc_id != CRTC_ID {
            return Err(VfsError::InvalidInput);
        }
        let legacy = dev.legacy_crtc.lock().clone();
        crtc.gamma_size = 0;
        if legacy.fb_id != 0 {
            crtc.x = legacy.x;
            crtc.y = legacy.y;
            crtc.fb_id = legacy.fb_id;
            crtc.mode_valid = legacy.mode_valid;
            crtc.mode = if legacy.mode_valid != 0 {
                legacy.mode
            } else {
                DrmModeModeInfo::default()
            };
            crtc.count_connectors = report_user_array(
                crtc.set_connectors_ptr,
                crtc.count_connectors,
                &legacy.connectors,
            )?;
        } else {
            crtc.x = 0;
            crtc.y = 0;
            crtc.fb_id = 0;
            crtc.mode_valid = 1;
            crtc.mode = current_mode();
            let empty: &[u32] = &[];
            crtc.count_connectors =
                report_user_array(crtc.set_connectors_ptr, crtc.count_connectors, empty)?;
        }
        Ok(0)
    }
}

#[allow(dead_code)]
struct DumbBuffer {
    width: u32,
    height: u32,
    bpp: u32,
    pitch: u32,
    size: u64,
    offset: u64,
    pages: Arc<GlobalPage>,
}

struct Framebuffer {
    size: u64,
    pages: Arc<GlobalPage>,
}

#[derive(Debug, Default, Clone)]
struct LegacyCrtcState {
    fb_id: u32,
    connectors: Vec<u32>,
    mode: DrmModeModeInfo,
    mode_valid: u32,
    x: u32,
    y: u32,
}

#[derive(Debug, Default, Clone, Copy)]
struct ModesetState {
    crtc_active: u64,
    crtc_mode_id: u32,
    conn_crtc_id: u32,
    plane_fb_id: u32,
    plane_crtc_id: u32,
    plane_src_x: u64,
    plane_src_y: u64,
    plane_src_w: u64,
    plane_src_h: u64,
    plane_crtc_x: i64,
    plane_crtc_y: i64,
    plane_crtc_w: u64,
    plane_crtc_h: u64,
}

/// The DRM character device implementing `/dev/dri/card0`.
pub struct Card0 {
    events: Mutex<VecDeque<DrmEventVblank>>,
    poll_rx: PollSet,
    sequence: AtomicU32,
    state: Mutex<ModesetState>,
    legacy_crtc: Mutex<LegacyCrtcState>,
    dumbs: Mutex<BTreeMap<u32, DumbBuffer>>,
    next_dumb_handle: AtomicU32,
    next_offset: AtomicU64,
    fbs: Mutex<BTreeMap<u32, Framebuffer>>,
    next_fb_id: AtomicU32,
    blobs: Mutex<BTreeMap<u32, Arc<Vec<u8>>>>,
    mode_id_blob_ref: Mutex<Option<Arc<Vec<u8>>>>,
    next_blob_id: AtomicU32,
    system_blobs: Mutex<BTreeMap<u32, Arc<Vec<u8>>>>,
    in_formats_blob: AtomicU32,
    system_blobs_init: Mutex<()>,
    retained_pages: Mutex<BTreeMap<u64, Arc<GlobalPage>>>,
}

impl Card0 {
    /// Create a new DRM device instance.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            events: Mutex::new(VecDeque::with_capacity(MAX_EVENTS)),
            poll_rx: PollSet::new(),
            sequence: AtomicU32::new(0),
            state: Mutex::new(ModesetState::default()),
            legacy_crtc: Mutex::new(LegacyCrtcState::default()),
            dumbs: Mutex::new(BTreeMap::new()),
            next_dumb_handle: AtomicU32::new(FIRST_DUMB_HANDLE),
            next_offset: AtomicU64::new(DUMB_BUFFER_OFFSET_STRIDE),
            fbs: Mutex::new(BTreeMap::new()),
            next_fb_id: AtomicU32::new(FIRST_FB_ID),
            blobs: Mutex::new(BTreeMap::new()),
            mode_id_blob_ref: Mutex::new(None),
            next_blob_id: AtomicU32::new(FIRST_BLOB_ID),
            system_blobs: Mutex::new(BTreeMap::new()),
            in_formats_blob: AtomicU32::new(0),
            system_blobs_init: Mutex::new(()),
            retained_pages: Mutex::new(BTreeMap::new()),
        })
    }

    fn ensure_in_formats_blob(&self) -> u32 {
        let cur = self.in_formats_blob.load(Ordering::Acquire);
        if cur != 0 {
            return cur;
        }
        let _guard = self.system_blobs_init.lock();
        let cur = self.in_formats_blob.load(Ordering::Acquire);
        if cur != 0 {
            return cur;
        }
        let bytes = build_in_formats_blob();
        let id = self.next_blob_id.fetch_add(1, Ordering::Relaxed);
        self.system_blobs.lock().insert(id, Arc::new(bytes));
        self.in_formats_blob.store(id, Ordering::Release);
        id
    }
}

fn report_user_array<T: UserWrite + Copy>(
    user_ptr: UserPtr<T>,
    cap: u32,
    src: &[T],
) -> VfsResult<u32> {
    if !user_ptr.is_null() {
        let to_write = (cap as usize).min(src.len());
        user_ptr
            .write_vm_slice(&src[..to_write])
            .map_err(|_| VfsError::BadAddress)?;
    }
    Ok(src.len() as u32)
}

fn display_resolution() -> (u32, u32) {
    if fbdevice::fb_available() {
        let info = fbdevice::fb_info();
        (info.width, info.height)
    } else {
        (640, 480)
    }
}

fn current_mode() -> DrmModeModeInfo {
    let (w, h) = display_resolution();
    let mut name = [0u8; 32];
    let s = b"current";
    name[..s.len()].copy_from_slice(s);

    let hdisplay = w as u16;
    let hsync_start = hdisplay + CVT_RB_HFRONT_PORCH;
    let hsync_end = hsync_start + CVT_RB_HSYNC_WIDTH;
    let htotal = hsync_end + CVT_RB_HBACK_PORCH;

    let vdisplay = h as u16;
    let vsync_start = vdisplay + CVT_RB_VFRONT_PORCH;
    let vsync_end = vsync_start + CVT_RB_VSYNC_WIDTH;
    let vtotal = vsync_end + CVT_RB_VBACK_PORCH;

    let vrefresh: u32 = DEFAULT_VREFRESH;
    let clock = ((htotal as u32) * (vtotal as u32) * vrefresh) / 1000;

    DrmModeModeInfo {
        clock,
        hdisplay,
        hsync_start,
        hsync_end,
        htotal,
        hskew: 0,
        vdisplay,
        vsync_start,
        vsync_end,
        vtotal,
        vscan: 0,
        vrefresh,
        flags: 0,
        kind: 0,
        name,
    }
}

impl DeviceFileOps for Card0 {
    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let evsz = core::mem::size_of::<DrmEventVblank>();
        if buf.len() < evsz {
            return Err(VfsError::InvalidInput);
        }
        let mut events = self.events.lock();
        let mut written = 0;
        while written + evsz <= buf.len() {
            let Some(ev) = events.pop_front() else {
                break;
            };
            buf[written..written + evsz].copy_from_slice(bytes_of(&ev));
            written += evsz;
        }
        if written == 0 {
            Err(VfsError::WouldBlock)
        } else {
            Ok(written)
        }
    }

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            DRM_IOCTL_SET_MASTER | DRM_IOCTL_DROP_MASTER => return Ok(0),
            _ => {}
        }

        route_ioctls! {
            cmd, arg, self; [
                DrmVersion,
                DrmUnique,
                DrmSetVersion,
                DrmGetCap,
                DrmSetClientCap,
                DrmAuth,
                DrmAuthMagic,
                DrmModeDirtyFB,
                DrmPrimeHandle,
                DrmPrimeFdToHandle,
                DrmModeCardRes,
                DrmModeCrtc,
                DrmModeSetCrtc,
                DrmModeGetEncoder,
                DrmModeGetConnector,
                DrmModeRmFb,
                DrmModeCrtcPageFlip,
                DrmModeCreateDumb,
                DrmModeMapDumb,
                DrmModeDestroyDumb,
                DrmModeGetPlaneRes,
                DrmModeGetPlane,
                DrmModeObjGetProperties,
                DrmModeGetProperty,
                DrmWaitVblank,
                DrmModeAtomic,
                DrmModeCreateBlob,
                DrmModeDestroyBlob,
                DrmModeGetBlob,
                DrmModeFbCmd2,
            ]
        }
    }

    fn mmap(&self, _file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let offset = mapper.offset() as u64;
        let dumbs = self.dumbs.lock();
        let Some(b) = dumbs.values().find(|b| b.offset == offset) else {
            return Err(VfsError::InvalidInput);
        };
        let phys = v2p(b.pages.start_va());
        let size = b.pages.size();
        self.retained_pages.lock().insert(offset, b.pages.clone());
        let range = PhysAddrRange::from_start_size(
            memaddr::PhysAddr::from(phys.as_usize().wrapping_sub(offset as usize)),
            size + offset as usize,
        );
        mapper.map_physical(range)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn poll(&self, _file: &VfsFile) -> IoEvents {
        Pollable::poll(self)
    }

    fn register_poll(
        &self,
        _file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Pollable::register(self, context, events)
    }
}

impl Pollable for Card0 {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, !self.events.lock().is_empty());
        events
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_rx)?;
        }
        Ok(())
    }
}

impl DrmIoctl for DrmModeSetCrtc {
    const CMD: u32 = iowr::<DrmModeCrtc>(DRM_TYPE, 0xA2);

    fn handle(dev: &Card0, crtc: &mut Self) -> VfsResult<usize> {
        let c = &crtc.0;
        if c.crtc_id != CRTC_ID {
            return Err(VfsError::InvalidInput);
        }
        if c.fb_id == 0 && c.count_connectors == 0 {
            *dev.legacy_crtc.lock() = LegacyCrtcState::default();
            return Ok(0);
        }
        if c.fb_id == 0 || !dev.fbs.lock().contains_key(&c.fb_id) {
            return Err(VfsError::InvalidInput);
        }
        if c.count_connectors == 0 || c.set_connectors_ptr.is_null() {
            return Err(VfsError::InvalidInput);
        }
        if c.count_connectors > 16 {
            return Err(VfsError::InvalidInput);
        }
        let connectors: Vec<u32> = c
            .set_connectors_ptr
            .load_vm_vec(c.count_connectors as usize)?;
        for &id in &connectors {
            if id != CONNECTOR_ID {
                return Err(VfsError::InvalidInput);
            }
        }
        *dev.legacy_crtc.lock() = LegacyCrtcState {
            fb_id: c.fb_id,
            connectors,
            mode: c.mode,
            mode_valid: c.mode_valid,
            x: c.x,
            y: c.y,
        };
        dev.present_fb(c.fb_id);
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetEncoder {
    const CMD: u32 = iowr::<DrmModeGetEncoder>(DRM_TYPE, 0xA6);

    fn handle(_dev: &Card0, e: &mut Self) -> VfsResult<usize> {
        if e.encoder_id != ENCODER_ID {
            return Err(VfsError::InvalidInput);
        }
        e.encoder_type = DRM_MODE_ENCODER_VIRTUAL;
        e.crtc_id = CRTC_ID;
        e.possible_crtcs = 1;
        e.possible_clones = 0;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetConnector {
    const CMD: u32 = iowr::<DrmModeGetConnector>(DRM_TYPE, 0xA7);

    fn handle(_dev: &Card0, c: &mut Self) -> VfsResult<usize> {
        if c.connector_id != CONNECTOR_ID {
            return Err(VfsError::InvalidInput);
        }
        c.encoder_id = ENCODER_ID;
        c.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
        c.connector_type_id = 1;
        c.connection = DRM_MODE_CONNECTED;
        let (w, h) = display_resolution();
        c.mm_width = w;
        c.mm_height = h;
        c.subpixel = 0;
        c.count_encoders = report_user_array(c.encoders_ptr, c.count_encoders, &[ENCODER_ID])?;
        if !c.modes_ptr.is_null() && c.count_modes > 0 {
            c.modes_ptr
                .write_vm(current_mode())
                .map_err(|_| VfsError::BadAddress)?;
        }
        c.count_modes = 1;
        c.count_props = 0;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeRmFb {
    const CMD: u32 = iowr::<u32>(DRM_TYPE, 0xAF);

    fn handle(dev: &Card0, fb_id: &mut Self) -> VfsResult<usize> {
        dev.fbs.lock().remove(&fb_id.0);
        {
            let mut legacy = dev.legacy_crtc.lock();
            if legacy.fb_id == fb_id.0 {
                *legacy = LegacyCrtcState::default();
            }
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCrtcPageFlip {
    const CMD: u32 = iowr::<DrmModeCrtcPageFlip>(DRM_TYPE, 0xB0);

    fn handle(dev: &Card0, f: &mut Self) -> VfsResult<usize> {
        if f.crtc_id != CRTC_ID || !dev.fbs.lock().contains_key(&f.fb_id) {
            return Err(VfsError::InvalidInput);
        }
        dev.present_fb(f.fb_id);
        if f.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            dev.queue_flip_event(f.user_data);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCreateDumb {
    const CMD: u32 = iowr::<DrmModeCreateDumb>(DRM_TYPE, 0xB2);

    fn handle(dev: &Card0, c: &mut Self) -> VfsResult<usize> {
        if c.width == 0
            || c.height == 0
            || c.bpp == 0
            || c.bpp > 64
            || !c.bpp.is_multiple_of(8)
            || c.flags != 0
        {
            return Err(VfsError::InvalidInput);
        }
        if c.width > 16384 || c.height > 16384 {
            return Err(VfsError::InvalidInput);
        }
        let bytes_per_pixel = c.bpp / 8;
        let pitch = c
            .width
            .checked_mul(bytes_per_pixel)
            .ok_or(VfsError::InvalidInput)?;
        let size = (pitch as u64)
            .checked_mul(c.height as u64)
            .ok_or(VfsError::InvalidInput)?;
        if size as usize > DUMB_BUFFER_MAX_SIZE {
            return Err(VfsError::NoMemory);
        }
        c.pitch = pitch;
        c.size = size;
        let size_aligned = (size as usize).next_multiple_of(PAGE_SIZE_4K);
        let pages = size_aligned / PAGE_SIZE_4K;
        let mut backing =
            GlobalPage::alloc_contiguous(pages, PAGE_SIZE_4K).map_err(|_| VfsError::NoMemory)?;
        backing.zero();
        let pages_arc = Arc::new(backing);
        let offset = dev
            .next_offset
            .fetch_add(DUMB_BUFFER_OFFSET_STRIDE, Ordering::Relaxed);
        let handle = dev.next_dumb_handle.fetch_add(1, Ordering::Relaxed);

        dev.dumbs.lock().insert(
            handle,
            DumbBuffer {
                width: c.width,
                height: c.height,
                bpp: c.bpp,
                pitch: c.pitch,
                size: c.size,
                offset,
                pages: pages_arc,
            },
        );
        c.handle = handle;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeMapDumb {
    const CMD: u32 = iowr::<DrmModeMapDumb>(DRM_TYPE, 0xB3);

    fn handle(dev: &Card0, m: &mut Self) -> VfsResult<usize> {
        let offset = dev
            .dumbs
            .lock()
            .get(&m.handle)
            .map(|b| b.offset)
            .ok_or(VfsError::InvalidInput)?;
        m.offset = offset;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeDestroyDumb {
    const CMD: u32 = iowr::<DrmModeDestroyDumb>(DRM_TYPE, 0xB4);

    fn handle(dev: &Card0, d: &mut Self) -> VfsResult<usize> {
        if let Some(buf) = dev.dumbs.lock().remove(&d.handle) {
            drop(buf);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetPlaneRes {
    const CMD: u32 = iowr::<DrmModeGetPlaneRes>(DRM_TYPE, 0xB5);

    fn handle(_dev: &Card0, r: &mut Self) -> VfsResult<usize> {
        let planes: &[u32] = &[PLANE_ID];
        r.count_planes = report_user_array(r.plane_id_ptr, r.count_planes, planes)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetPlane {
    const CMD: u32 = iowr::<DrmModeGetPlane>(DRM_TYPE, 0xB6);

    fn handle(_dev: &Card0, p: &mut Self) -> VfsResult<usize> {
        if p.plane_id != PLANE_ID {
            return Err(VfsError::InvalidInput);
        }
        p.crtc_id = CRTC_ID;
        p.fb_id = 0;
        p.possible_crtcs = 1;
        p.gamma_size = 0;
        p.count_format_types =
            report_user_array(p.format_type_ptr, p.count_format_types, SUPPORTED_FORMATS)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeObjGetProperties {
    const CMD: u32 = iowr::<DrmModeObjGetProperties>(DRM_TYPE, 0xB9);

    fn handle(dev: &Card0, q: &mut Self) -> VfsResult<usize> {
        let state = *dev.state.lock();
        let (prop_ids, prop_vals): (&[u32], Vec<u64>) = match (q.obj_type, q.obj_id) {
            (DRM_MODE_OBJECT_PLANE, PLANE_ID) => {
                let blob_id = dev.ensure_in_formats_blob() as u64;
                (PLANE_PROPS, plane_prop_values(&state, blob_id))
            }
            (DRM_MODE_OBJECT_CRTC, CRTC_ID) => (CRTC_PROPS, crtc_prop_values(&state)),
            (DRM_MODE_OBJECT_CONNECTOR, CONNECTOR_ID) => (CONN_PROPS, conn_prop_values(&state)),
            _ => return Err(VfsError::NotFound),
        };
        report_user_array(q.props_ptr, q.count_props, prop_ids)?;
        report_user_array(q.prop_values_ptr, q.count_props, &prop_vals)?;
        q.count_props = prop_ids.len() as u32;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetProperty {
    const CMD: u32 = iowr::<DrmModeGetProperty>(DRM_TYPE, 0xAA);

    fn handle(_dev: &Card0, g: &mut Self) -> VfsResult<usize> {
        let meta = property_meta(g.prop_id).ok_or(VfsError::NotFound)?;

        g.flags = meta.flags;
        g.name = [0; DRM_PROP_NAME_LEN];
        let nb = meta.name.as_bytes();
        let n = nb.len().min(DRM_PROP_NAME_LEN - 1);
        g.name[..n].copy_from_slice(&nb[..n]);

        match meta.kind {
            PropKind::Enum(enums) => {
                g.count_values = enums.len() as u32;
                g.count_enum_blobs = report_user_array(g.enum_blob_ptr, g.count_enum_blobs, enums)?;
            }
            PropKind::RangeU64 { min, max } => {
                let limits = [min, max];
                g.count_values = report_user_array(g.values_ptr, g.count_values, &limits)?;
                g.count_enum_blobs = 0;
            }
            PropKind::Object | PropKind::Blob => {
                g.count_values = 0;
                g.count_enum_blobs = 0;
            }
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmWaitVblank {
    const CMD: u32 = iowr::<DrmWaitVblank>(DRM_TYPE, 0x3A);

    fn handle(dev: &Card0, request: &mut Self) -> VfsResult<usize> {
        let is_relative = request.rep_type & DRM_VBLANK_RELATIVE != 0;
        let current = dev.sequence.load(Ordering::Acquire);
        let target = if is_relative {
            current.wrapping_add(request.sequence)
        } else {
            request.sequence
        };
        let raw_wait = target.wrapping_sub(current);
        let wait_count = if raw_wait == 0 || raw_wait >= i32::MAX as u32 {
            1
        } else {
            raw_wait
        };

        const FRAME_PERIOD_NS: u64 = 1_000_000_000 / 60;
        let delay =
            core::time::Duration::from_nanos(FRAME_PERIOD_NS.saturating_mul(wait_count as u64));
        ktask::sleep(delay);
        dev.sequence.fetch_add(wait_count, Ordering::AcqRel);

        let now = khal::time::monotonic_time();
        *request = DrmWaitVblank {
            rep_type: 0,
            sequence: dev.sequence.load(Ordering::Acquire),
            tv_sec: now.as_secs() as i64,
            tv_usec: now.subsec_micros() as i64,
        };
        Ok(0)
    }
}

impl DrmIoctl for DrmModeAtomic {
    const CMD: u32 = iowr::<DrmModeAtomic>(DRM_TYPE, 0xBC);

    fn handle(dev: &Card0, a: &mut Self) -> VfsResult<usize> {
        let known = DRM_MODE_ATOMIC_TEST_ONLY
            | DRM_MODE_ATOMIC_NONBLOCK
            | DRM_MODE_ATOMIC_ALLOW_MODESET
            | DRM_MODE_PAGE_FLIP_EVENT;
        if a.flags & !known != 0 {
            return Err(VfsError::InvalidInput);
        }

        let n = a.count_objs as usize;
        let objs: Vec<u32> = a.objs_ptr.load_vm_vec(n)?;
        let counts: Vec<u32> = a.count_props_ptr.load_vm_vec(n)?;
        let total_props: usize = counts.iter().map(|c| *c as usize).sum();
        let props: Vec<u32> = a.props_ptr.load_vm_vec(total_props)?;
        let values: Vec<u64> = a.prop_values_ptr.load_vm_vec(total_props)?;

        let mut state = dev.state.lock();
        let mut proposed = *state;
        let mut new_mode_blob: Option<Option<Arc<Vec<u8>>>> = None;
        let mut idx = 0;
        for (obj_i, &obj_id) in objs.iter().enumerate() {
            let obj_type = object_type_of(obj_id).ok_or(VfsError::NotFound)?;
            for _ in 0..counts[obj_i] {
                let prop_id = props[idx];
                let value = values[idx];
                idx += 1;
                if !dev.apply_prop(
                    obj_type,
                    obj_id,
                    prop_id,
                    value,
                    &mut proposed,
                    &mut new_mode_blob,
                )? {
                    return Err(VfsError::InvalidInput);
                }
            }
        }

        if a.flags & DRM_MODE_ATOMIC_TEST_ONLY != 0 {
            return Ok(0);
        }

        let current_fb = proposed.plane_fb_id;
        *state = proposed;
        drop(state);
        if let Some(new_ref) = new_mode_blob {
            *dev.mode_id_blob_ref.lock() = new_ref;
        }
        if current_fb != 0 {
            dev.present_fb(current_fb);
        }
        if a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            dev.queue_flip_event(a.user_data);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCreateBlob {
    const CMD: u32 = iowr::<DrmModeCreateBlob>(DRM_TYPE, 0xBD);

    fn handle(dev: &Card0, c: &mut Self) -> VfsResult<usize> {
        if c.length == 0 || c.length as usize > MAX_BLOB_BYTES {
            return Err(VfsError::InvalidInput);
        }
        let len = c.length as usize;
        let bytes = c.data.load_vm_vec(len).map_err(|_| VfsError::BadAddress)?;
        let id = dev.next_blob_id.fetch_add(1, Ordering::Relaxed);
        dev.blobs.lock().insert(id, Arc::new(bytes));
        c.blob_id = id;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeDestroyBlob {
    const CMD: u32 = iowr::<DrmModeDestroyBlob>(DRM_TYPE, 0xBE);

    fn handle(dev: &Card0, d: &mut Self) -> VfsResult<usize> {
        if dev.system_blobs.lock().contains_key(&d.blob_id) {
            return Err(VfsError::PermissionDenied);
        }
        dev.blobs
            .lock()
            .remove(&d.blob_id)
            .ok_or(VfsError::NotFound)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetBlob {
    const CMD: u32 = iowr::<DrmModeGetBlob>(DRM_TYPE, 0xAC);

    fn handle(dev: &Card0, g: &mut Self) -> VfsResult<usize> {
        let bytes = if let Some(b) = dev.blobs.lock().get(&g.blob_id).cloned() {
            b
        } else if g.blob_id == dev.state.lock().crtc_mode_id
            && let Some(b) = dev.mode_id_blob_ref.lock().clone()
        {
            b
        } else if let Some(b) = dev.system_blobs.lock().get(&g.blob_id).cloned() {
            b
        } else {
            return Err(VfsError::NotFound);
        };
        if !g.data.is_null() && g.length > 0 {
            let n = (g.length as usize).min(bytes.len());
            g.data
                .write_vm_slice(&bytes[..n])
                .map_err(|_| VfsError::BadAddress)?;
        }
        g.length = bytes.len() as u32;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeFbCmd2 {
    const CMD: u32 = iowr::<DrmModeFbCmd2>(DRM_TYPE, 0xB8);

    fn handle(dev: &Card0, f: &mut Self) -> VfsResult<usize> {
        let handle = f.handles[0];
        let (pages, size) = {
            let dumbs = dev.dumbs.lock();
            let Some(b) = dumbs.get(&handle) else {
                return Err(VfsError::InvalidInput);
            };
            (b.pages.clone(), b.size)
        };
        if f.flags & DRM_MODE_FB_MODIFIERS != 0 {
            for i in 0..4 {
                if f.handles[i] == 0 {
                    continue;
                }
                let m = f.modifier[i];
                if m != DRM_FORMAT_MOD_LINEAR && m != DRM_FORMAT_MOD_INVALID {
                    return Err(VfsError::InvalidInput);
                }
            }
        }
        let fb_id = dev.next_fb_id.fetch_add(1, Ordering::Relaxed);
        dev.fbs.lock().insert(fb_id, Framebuffer { size, pages });
        f.fb_id = fb_id;
        Ok(0)
    }
}

impl Card0 {
    fn present_fb(&self, fb_id: u32) {
        let (pages, size) = match self.fbs.lock().get(&fb_id) {
            Some(fb) => (fb.pages.clone(), fb.size),
            None => return,
        };
        if !fbdevice::fb_available() {
            return;
        };
        let info = fbdevice::fb_info();
        let copy = (size as usize).min(info.fb_size);
        // SAFETY: both framebuffer regions are valid for `copy` bytes and do
        // not overlap: one is the DRM shadow buffer, the other is the mapped
        // display framebuffer.
        unsafe {
            core::ptr::copy_nonoverlapping(
                pages.start_va().as_usize() as *const u8,
                info.fb_base_vaddr as *mut u8,
                copy,
            );
        }
        let _ = fbdevice::fb_flush();
    }

    fn queue_flip_event(&self, user_data: u64) {
        let seq = self
            .sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let now = khal::time::monotonic_time();
        let ev = DrmEventVblank {
            base: DrmEvent {
                event_type: DRM_EVENT_FLIP_COMPLETE,
                length: core::mem::size_of::<DrmEventVblank>() as u32,
            },
            user_data,
            tv_sec: now.as_secs() as u32,
            tv_usec: now.subsec_micros(),
            sequence: seq,
            crtc_id: CRTC_ID,
        };
        let enqueued = {
            let mut queue = self.events.lock();
            if queue.len() >= MAX_EVENTS {
                false
            } else {
                queue.push_back(ev);
                true
            }
        };
        if enqueued {
            self.poll_rx.wake();
        }
    }

    fn apply_prop(
        &self,
        obj_type: u32,
        _obj_id: u32,
        prop_id: u32,
        value: u64,
        s: &mut ModesetState,
        new_mode_blob: &mut Option<Option<Arc<Vec<u8>>>>,
    ) -> VfsResult<bool> {
        match (obj_type, prop_id) {
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_TYPE) => {
                if value != DRM_PLANE_TYPE_PRIMARY {
                    return Err(VfsError::InvalidInput);
                }
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_FB_ID) => {
                let fb = value as u32;
                if fb != 0 && !self.fbs.lock().contains_key(&fb) {
                    return Err(VfsError::InvalidInput);
                }
                s.plane_fb_id = fb;
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_ID) => {
                let c = value as u32;
                if c != 0 && c != CRTC_ID {
                    return Err(VfsError::InvalidInput);
                }
                s.plane_crtc_id = c;
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_SRC_X) => s.plane_src_x = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_SRC_Y) => s.plane_src_y = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_SRC_W) => s.plane_src_w = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_SRC_H) => s.plane_src_h = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_X) => {
                s.plane_crtc_x = checked_i32(value)? as i64;
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_Y) => {
                s.plane_crtc_y = checked_i32(value)? as i64;
            }
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_W) => s.plane_crtc_w = value,
            (DRM_MODE_OBJECT_PLANE, PROP_PLANE_CRTC_H) => s.plane_crtc_h = value,
            (DRM_MODE_OBJECT_CRTC, PROP_CRTC_ACTIVE) => {
                if value > 1 {
                    return Err(VfsError::InvalidInput);
                }
                s.crtc_active = value;
            }
            (DRM_MODE_OBJECT_CRTC, PROP_CRTC_MODE_ID) => {
                let blob = value as u32;
                let arc = if blob == 0 {
                    None
                } else {
                    let arc = self.blobs.lock().get(&blob).cloned().or_else(|| {
                        if s.crtc_mode_id == blob {
                            self.mode_id_blob_ref.lock().clone()
                        } else {
                            None
                        }
                    });
                    Some(arc.ok_or(VfsError::InvalidInput)?)
                };
                s.crtc_mode_id = blob;
                *new_mode_blob = Some(arc);
            }
            (DRM_MODE_OBJECT_CONNECTOR, PROP_CONN_CRTC_ID) => {
                let c = value as u32;
                if c != 0 && c != CRTC_ID {
                    return Err(VfsError::InvalidInput);
                }
                s.conn_crtc_id = c;
            }
            _ => return Ok(false),
        }
        Ok(true)
    }
}

fn plane_prop_values(s: &ModesetState, in_formats: u64) -> Vec<u64> {
    vec![
        DRM_PLANE_TYPE_PRIMARY,
        s.plane_fb_id as u64,
        s.plane_crtc_id as u64,
        s.plane_src_x,
        s.plane_src_y,
        s.plane_src_w,
        s.plane_src_h,
        s.plane_crtc_x as u64,
        s.plane_crtc_y as u64,
        s.plane_crtc_w,
        s.plane_crtc_h,
        in_formats,
    ]
}

fn build_in_formats_blob() -> Vec<u8> {
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::NoUninit)]
    struct Header {
        version: u32,
        flags: u32,
        count_formats: u32,
        formats_offset: u32,
        count_modifiers: u32,
        modifiers_offset: u32,
    }
    #[repr(C)]
    #[derive(Clone, Copy, bytemuck::NoUninit)]
    struct ModifierEntry {
        formats: u64,
        offset: u32,
        _pad: u32,
        modifier: u64,
    }
    let n_formats = SUPPORTED_FORMATS.len() as u32;
    let formats_off = core::mem::size_of::<Header>() as u32;
    let modifiers_off = formats_off + n_formats * 4;
    let hdr = Header {
        version: 1,
        flags: 0,
        count_formats: n_formats,
        formats_offset: formats_off,
        count_modifiers: 1,
        modifiers_offset: modifiers_off,
    };
    let format_mask = (1u64 << n_formats) - 1;
    let me = ModifierEntry {
        formats: format_mask,
        offset: 0,
        _pad: 0,
        modifier: DRM_FORMAT_MOD_LINEAR,
    };
    let mut buf = Vec::with_capacity(
        core::mem::size_of::<Header>()
            + (n_formats as usize) * 4
            + core::mem::size_of::<ModifierEntry>(),
    );
    buf.extend_from_slice(bytes_of(&hdr));
    for fmt in SUPPORTED_FORMATS {
        buf.extend_from_slice(&fmt.to_le_bytes());
    }
    buf.extend_from_slice(bytes_of(&me));
    buf
}

fn crtc_prop_values(s: &ModesetState) -> Vec<u64> {
    vec![s.crtc_active, s.crtc_mode_id as u64]
}

fn conn_prop_values(s: &ModesetState) -> Vec<u64> {
    vec![s.conn_crtc_id as u64]
}

struct PropMeta {
    name: &'static str,
    flags: u32,
    kind: PropKind,
}

enum PropKind {
    Enum(&'static [DrmModePropertyEnum]),
    RangeU64 { min: u64, max: u64 },
    Object,
    Blob,
}

const fn enum_entry(value: u64, name: &[u8]) -> DrmModePropertyEnum {
    let mut e = DrmModePropertyEnum {
        value,
        name: [0; DRM_PROP_NAME_LEN],
    };
    let n = if name.len() < DRM_PROP_NAME_LEN - 1 {
        name.len()
    } else {
        DRM_PROP_NAME_LEN - 1
    };
    let mut i = 0;
    while i < n {
        e.name[i] = name[i];
        i += 1;
    }
    e
}

const PLANE_TYPE_ENUMS: &[DrmModePropertyEnum] = &[
    enum_entry(0, b"Overlay"),
    enum_entry(1, b"Primary"),
    enum_entry(2, b"Cursor"),
];

fn property_meta(id: u32) -> Option<PropMeta> {
    let atomic = DRM_MODE_PROP_ATOMIC;
    let meta = match id {
        PROP_PLANE_TYPE => PropMeta {
            name: "type",
            flags: DRM_MODE_PROP_ENUM | DRM_MODE_PROP_IMMUTABLE,
            kind: PropKind::Enum(PLANE_TYPE_ENUMS),
        },
        PROP_PLANE_FB_ID => PropMeta {
            name: "FB_ID",
            flags: DRM_MODE_PROP_OBJECT | atomic,
            kind: PropKind::Object,
        },
        PROP_PLANE_CRTC_ID => PropMeta {
            name: "CRTC_ID",
            flags: DRM_MODE_PROP_OBJECT | atomic,
            kind: PropKind::Object,
        },
        PROP_PLANE_SRC_X => range_u32("SRC_X", atomic),
        PROP_PLANE_SRC_Y => range_u32("SRC_Y", atomic),
        PROP_PLANE_SRC_W => range_u32("SRC_W", atomic),
        PROP_PLANE_SRC_H => range_u32("SRC_H", atomic),
        PROP_PLANE_CRTC_X => range_u32("CRTC_X", atomic),
        PROP_PLANE_CRTC_Y => range_u32("CRTC_Y", atomic),
        PROP_PLANE_CRTC_W => range_u32("CRTC_W", atomic),
        PROP_PLANE_CRTC_H => range_u32("CRTC_H", atomic),
        PROP_PLANE_IN_FORMATS => PropMeta {
            name: "IN_FORMATS",
            flags: DRM_MODE_PROP_BLOB | DRM_MODE_PROP_IMMUTABLE,
            kind: PropKind::Blob,
        },
        PROP_CRTC_ACTIVE => PropMeta {
            name: "ACTIVE",
            flags: DRM_MODE_PROP_RANGE | atomic,
            kind: PropKind::RangeU64 { min: 0, max: 1 },
        },
        PROP_CRTC_MODE_ID => PropMeta {
            name: "MODE_ID",
            flags: DRM_MODE_PROP_BLOB | atomic,
            kind: PropKind::Blob,
        },
        PROP_CONN_CRTC_ID => PropMeta {
            name: "CRTC_ID",
            flags: DRM_MODE_PROP_OBJECT | atomic,
            kind: PropKind::Object,
        },
        _ => return None,
    };
    Some(meta)
}

fn range_u32(name: &'static str, atomic: u32) -> PropMeta {
    PropMeta {
        name,
        flags: DRM_MODE_PROP_RANGE | atomic,
        kind: PropKind::RangeU64 {
            min: 0,
            max: u32::MAX as u64,
        },
    }
}

fn object_type_of(id: u32) -> Option<u32> {
    match id {
        CRTC_ID => Some(DRM_MODE_OBJECT_CRTC),
        CONNECTOR_ID => Some(DRM_MODE_OBJECT_CONNECTOR),
        PLANE_ID => Some(DRM_MODE_OBJECT_PLANE),
        _ => None,
    }
}

fn checked_i32(value: u64) -> VfsResult<i32> {
    let v = value as i64;
    if (i32::MIN as i64..=i32::MAX as i64).contains(&v) {
        Ok(v as i32)
    } else {
        Err(VfsError::InvalidInput)
    }
}
