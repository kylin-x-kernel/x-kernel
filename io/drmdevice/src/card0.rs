// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal DRM primary character device.
//!
//! Single-CRTC, single-connector, single-plane resource-backed DRM/KMS driver.
//! Covers legacy libdrm (`CREATE_DUMB → ADDFB2 → SETCRTC → PAGE_FLIP`) and the
//! atomic-KMS path (`MODE_ATOMIC` + blob properties) used by modern compositors.

use alloc::{
    collections::{BTreeMap, BTreeSet, VecDeque},
    format,
    string::String,
    sync::{Arc, Weak},
    vec,
    vec::Vec,
};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use bytemuck::bytes_of;
use display::{DisplayDevice, ScanoutFormat, ScanoutRect, ScanoutResource};
use kalloc::GlobalPage;
use kclass::{ClassDevice, DisplayDeviceImpl};
use khal::mem::v2p;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use kvfs::{
    DeviceFileOps, MmapMapper, NodeFlags, VfsError, VfsFile, VfsFileBuilder, VfsInode, VfsResult,
};
use memaddr::{PAGE_SIZE_4K, PhysAddrRange};
use posix_types::{UserPtr, UserRead, UserWrite};

use super::{consts::*, drm::*};

/// DRM ioctl handler trait. `const CMD` is injected by the `#[drm_ioctl]` attribute macro.
trait DrmIoctl: Sized {
    const CMD: u32;
    fn handle(dev: &DrmCard, file: &Arc<DrmFile>, arg: &mut Self) -> VfsResult<usize>;

    fn handle_raw(dev: &DrmCard, file: &Arc<DrmFile>, arg: UserPtr<Self>) -> VfsResult<usize>
    where
        Self: UserRead + UserWrite,
    {
        const { assert!(Self::CMD >> 30 == (IOC_READ | IOC_WRITE)) };
        let mut ptr: Self = arg.read_vm().map_err(|_| kvfs::VfsError::BadAddress)?;
        let result = Self::handle(dev, file, &mut ptr);
        arg.write_vm(ptr).map_err(|_| kvfs::VfsError::BadAddress)?;
        result
    }

    fn handle_raw_output(dev: &DrmCard, file: &Arc<DrmFile>, arg: UserPtr<Self>) -> VfsResult<usize>
    where
        Self: Default + UserWrite,
    {
        const { assert!(Self::CMD >> 30 == IOC_READ) };
        let mut value = Self::default();
        let result = Self::handle(dev, file, &mut value);
        arg.write_vm(value).map_err(|_| VfsError::BadAddress)?;
        result
    }

    fn handle_raw_input(dev: &DrmCard, file: &Arc<DrmFile>, arg: UserPtr<Self>) -> VfsResult<usize>
    where
        Self: UserRead,
    {
        const { assert!(Self::CMD >> 30 == IOC_WRITE) };
        let mut value: Self = arg.read_vm().map_err(|_| VfsError::BadAddress)?;
        Self::handle(dev, file, &mut value)
    }
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

// VERSION reports full lengths even for a length-only or truncated query.
// Capture the original capacity before updating the output length; this ioctl
// does not append a NUL terminator.
fn copy_version_field(user_ptr: UserPtr<u8>, capacity: usize, value: &str) -> VfsResult<usize> {
    let copy_len = capacity.min(value.len());
    if copy_len != 0 && !user_ptr.is_null() {
        user_ptr
            .write_vm_slice(&value.as_bytes()[..copy_len])
            .map_err(|_| VfsError::BadAddress)?;
    }
    Ok(value.len())
}

impl DrmIoctl for DrmVersion {
    const CMD: u32 = iowr::<DrmVersion>(DRM_TYPE, 0x00);

    fn handle(_dev: &DrmCard, _file: &Arc<DrmFile>, version: &mut Self) -> VfsResult<usize> {
        version.version_major = DRIVER_VERSION_MAJOR;
        version.version_minor = DRIVER_VERSION_MINOR;
        version.version_patchlevel = DRIVER_VERSION_PATCHLEVEL;
        version.name_len = copy_version_field(version.name, version.name_len, DRIVER_NAME)?;
        version.date_len = copy_version_field(version.date, version.date_len, DRIVER_DATE)?;
        version.desc_len = copy_version_field(version.desc, version.desc_len, DRIVER_DESC)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmSetVersion {
    const CMD: u32 = iowr::<DrmSetVersion>(DRM_TYPE, 0x07);

    fn handle(dev: &DrmCard, file: &Arc<DrmFile>, set_version: &mut Self) -> VfsResult<usize> {
        let requested = *set_version;
        // Like Linux drm_setversion's done path, return supported versions even
        // for lower-version requests and validation errors, not the input pair.
        set_version.drm_di_major = 1;
        set_version.drm_di_minor = 4;
        set_version.drm_dd_major = DRIVER_VERSION_MAJOR;
        set_version.drm_dd_minor = DRIVER_VERSION_MINOR;

        let masters = dev.masters.lock();
        if !masters
            .active
            .as_ref()
            .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(file)))
        {
            return Err(VfsError::PermissionDenied);
        }
        let binding = file.binding.lock();
        // Linux treats interface major -1 as a query and ignores its minor.
        // See drm_setversion in https://github.com/torvalds/linux/blob/master/drivers/gpu/drm/drm_ioctl.c.
        if requested.drm_di_major != -1 {
            if requested.drm_di_major != 1 || !(0..=4).contains(&requested.drm_di_minor) {
                return Err(VfsError::InvalidInput);
            }
            if requested.drm_di_minor >= 1 {
                *binding.master.unique.lock() = Some(format!("{}:0", DRIVER_NAME));
            }
        }
        // Linux establishes the bus identity before validating the driver
        // version, and still copies supported versions out on an error.
        if requested.drm_dd_major != -1
            && (requested.drm_dd_major != DRIVER_VERSION_MAJOR
                || requested.drm_dd_minor < 0
                || requested.drm_dd_minor > DRIVER_VERSION_MINOR)
        {
            return Err(VfsError::InvalidInput);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmUnique {
    const CMD: u32 = iowr::<DrmUnique>(DRM_TYPE, 0x01);

    fn handle(dev: &DrmCard, file: &Arc<DrmFile>, unique: &mut Self) -> VfsResult<usize> {
        let identity = {
            let _masters = dev.masters.lock();
            let binding = file.binding.lock();
            binding.master.unique.lock().clone()
        };
        let unique_str = identity.as_deref().unwrap_or("");
        // GET_UNIQUE copies only when the complete identity fits, unlike VERSION.
        if !unique_str.is_empty() && unique.unique_len >= unique_str.len() {
            unique
                .unique
                .write_vm_slice(unique_str.as_bytes())
                .map_err(|_| VfsError::BadAddress)?;
        }
        unique.unique_len = unique_str.len();
        Ok(0)
    }
}

impl DrmIoctl for DrmGetCap {
    const CMD: u32 = iowr::<DrmGetCap>(DRM_TYPE, 0x0c);

    fn handle(_dev: &DrmCard, _file: &Arc<DrmFile>, cap: &mut Self) -> VfsResult<usize> {
        cap.value = match cap.capability {
            DRM_CAP_DUMB_BUFFER => 1,
            // The driver exposes a single primary plane and no cursor
            // commands (the virtio-gpu cursor path is disabled), so these
            // are client-compatibility values: libdrm-based clients treat
            // 0 as "cursor unsupported" and bail out during init. See
            // DRM_CURSOR_COMPAT_SIZE in consts.rs.
            DRM_CAP_CURSOR_WIDTH | DRM_CAP_CURSOR_HEIGHT => DRM_CURSOR_COMPAT_SIZE,
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

    fn handle(_dev: &DrmCard, file: &Arc<DrmFile>, cap: &mut Self) -> VfsResult<usize> {
        // Mirror Linux `drm_setclientcap()`: reject an unknown capability with
        // EOPNOTSUPP and an out-of-range boolean with EINVAL, then remember what
        // was granted.
        //
        // Answering Ok(0) to everything - which is what this used to do - turns
        // every "does the driver support X?" probe into a false positive. Clients
        // legitimately branch on this: a libdrm client that is told `atomic` was
        // accepted will go on to issue atomic commits.  The granted bits are read
        // back by `DrmModeAtomic`; see `DrmFile::client_caps` for why the other
        // two are recorded rather than enforced.
        //
        // `STEREO_3D` and `UNIVERSAL_PLANES` are accepted even though nothing here
        // implements stereoscopy or gated plane enumeration.  Linux's
        // `drm_setclientcap()` does not ask the driver either: both are per-file
        // client flags that govern what the client itself may send, so a driver
        // that answered EOPNOTSUPP would be the one out of spec -- and a client
        // that probes them before doing anything else would give up.  Only
        // `ATOMIC` changes what this kernel acts on.
        match cap.capability {
            DRM_CLIENT_CAP_STEREO_3D | DRM_CLIENT_CAP_UNIVERSAL_PLANES | DRM_CLIENT_CAP_ATOMIC => {}
            _ => return Err(VfsError::OperationNotSupported),
        }
        if cap.value > 1 {
            return Err(VfsError::InvalidInput);
        }
        // Capability values are small and defined above, so the shift is in
        // range by construction.
        let bit = 1u64 << cap.capability;
        if cap.value == 1 {
            file.client_caps.fetch_or(bit, Ordering::Relaxed);
        } else {
            file.client_caps.fetch_and(!bit, Ordering::Relaxed);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmAuth {
    const CMD: u32 = ior::<DrmAuth>(DRM_TYPE, 0x02);

    fn handle(dev: &DrmCard, file: &Arc<DrmFile>, auth: &mut Self) -> VfsResult<usize> {
        let masters = dev.masters.lock();
        let binding = file.binding.lock();
        let mut file_magic = file.magic.lock();
        if !masters
            .active
            .as_ref()
            .is_some_and(|active| Arc::ptr_eq(&binding.master, &active.master))
        {
            if let Some(magic) = file_magic.take() {
                let mut registry = binding.master.magic_registry.lock();
                if registry
                    .files
                    .get(&magic)
                    .is_some_and(|owner| Weak::ptr_eq(owner, &Arc::downgrade(file)))
                {
                    registry.files.remove(&magic);
                }
            }
            return Err(VfsError::PermissionDenied);
        }
        let mut registry = binding.master.magic_registry.lock();
        if let Some(magic) = *file_magic {
            if registry
                .files
                .get(&magic)
                .is_some_and(|owner| Weak::ptr_eq(owner, &Arc::downgrade(file)))
            {
                auth.magic = magic;
                return Ok(0);
            }
            *file_magic = None;
        }

        let magic = registry.allocate(file).ok_or(VfsError::NoMemory)?;
        *file_magic = Some(magic);
        auth.magic = magic;
        Ok(0)
    }
}

impl DrmIoctl for DrmAuthMagic {
    const CMD: u32 = iow::<DrmAuth>(DRM_TYPE, 0x11);

    fn handle(dev: &DrmCard, file: &Arc<DrmFile>, auth: &mut Self) -> VfsResult<usize> {
        // Keep active-master ownership stable through token consumption. The
        // target Arc is declared outside the lock scope so its final drop
        // cannot re-enter DrmFile::drop while the master-state lock is held.
        let _target;
        {
            let masters = dev.masters.lock();
            let active = masters
                .active
                .as_ref()
                .filter(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(file)))
                .ok_or(VfsError::PermissionDenied)?;
            _target = active
                .master
                .magic_registry
                .lock()
                .files
                .remove(&auth.0.magic)
                .and_then(|target| target.upgrade())
                .ok_or(VfsError::InvalidInput)?;
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeDirtyFb {
    const CMD: u32 = iowr::<DrmModeDirtyFb>(DRM_TYPE, 0xB1);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, dirty: &mut Self) -> VfsResult<usize> {
        // Linux's drm_mode_dirtyfb_ioctl() reports an unknown fb_id as -ENOENT
        // (drm_framebuffer_lookup), and callers distinguish the two: Xorg keeps
        // its damage tracking enabled only when the generic path answers with
        // ENOENT rather than EINVAL. present_fb is best-effort, so validate the
        // identifier before calling it.
        // Serialize with `MODE_CLOSEFB`; see `MODE_CRTC_PAGE_FLIP`.
        let _commit = dev.commit_lock.lock();
        if !dev.fbs.lock().contains_key(&dirty.fb_id) {
            return Err(VfsError::NotFound);
        }
        dev.present_fb(dirty.fb_id);
        Ok(0)
    }
}

impl DrmIoctl for DrmPrimeHandle {
    const CMD: u32 = iowr::<DrmPrimeHandle>(DRM_TYPE, 0x2d);

    fn handle(_dev: &DrmCard, _file: &Arc<DrmFile>, prime: &mut Self) -> VfsResult<usize> {
        prime.fd = prime.handle as i32;
        Ok(0)
    }
}

impl DrmIoctl for DrmPrimeFdToHandle {
    const CMD: u32 = iowr::<DrmPrimeHandle>(DRM_TYPE, 0x2e);

    fn handle(_dev: &DrmCard, _file: &Arc<DrmFile>, prime: &mut Self) -> VfsResult<usize> {
        prime.0.handle = prime.0.fd as u32;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCardRes {
    const CMD: u32 = iowr::<DrmModeCardRes>(DRM_TYPE, 0xA0);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, r: &mut Self) -> VfsResult<usize> {
        let (w, h) = display_resolution(dev);
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

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, crtc: &mut Self) -> VfsResult<usize> {
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
            crtc.mode = current_mode(dev);
            let empty: &[u32] = &[];
            crtc.count_connectors =
                report_user_array(crtc.set_connectors_ptr, crtc.count_connectors, empty)?;
        }
        Ok(0)
    }
}

/// The fields of a legacy cursor request that this driver acts on.
///
/// `MODE_CURSOR` and `MODE_CURSOR2` differ only by the hotspot offsets, which a
/// driver with no hardware cursor plane has nothing to apply, so both handlers
/// reduce to the same request.  Grouping them also keeps `set_cursor` under
/// clippy's argument-count threshold.
#[derive(Debug, Clone, Copy)]
struct CursorRequest {
    crtc_id: u32,
    flags: u32,
    width: u32,
    height: u32,
    handle: u32,
}

struct DumbBuffer {
    width: u32,
    height: u32,
    pitch: u32,
    offset: u64,
    resource_id: u32,
    pages: Arc<GlobalPage>,
}

struct Framebuffer {
    resource_id: u32,
    width: u32,
    height: u32,
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

struct DrmMaster {
    unique: Mutex<Option<String>>,
    magic_registry: Mutex<DrmMagicRegistry>,
}

impl Default for DrmMaster {
    fn default() -> Self {
        Self {
            unique: Mutex::new(None),
            magic_registry: Mutex::new(DrmMagicRegistry::default()),
        }
    }
}

#[derive(Default)]
struct DrmMagicRegistry {
    next_magic: u32,
    files: BTreeMap<u32, Weak<DrmFile>>,
}

impl DrmMagicRegistry {
    fn allocate(&mut self, file: &Arc<DrmFile>) -> Option<u32> {
        let first = self.next_magic.max(1);
        let mut magic = first;
        loop {
            if let alloc::collections::btree_map::Entry::Vacant(entry) = self.files.entry(magic) {
                entry.insert(Arc::downgrade(file));
                self.next_magic = magic.wrapping_add(1).max(1);
                return Some(magic);
            }
            magic = magic.wrapping_add(1).max(1);
            if magic == first {
                return None;
            }
        }
    }
}

fn invalidate_magic_tokens(master: &DrmMaster) {
    master.magic_registry.lock().files.clear();
}

struct MasterBinding {
    master: Arc<DrmMaster>,
    // A former owner can reacquire its master after DROP_MASTER.
    is_master: bool,
}

struct ActiveMaster {
    master: Arc<DrmMaster>,
    owner: Weak<DrmFile>,
}

#[derive(Default)]
struct MasterState {
    active: Option<ActiveMaster>,
}

struct DrmFile {
    masters: Arc<Mutex<MasterState>>,
    binding: Mutex<MasterBinding>,
    /// Capabilities granted through `DRM_IOCTL_SET_CLIENT_CAP`, as a bitmask
    /// indexed by the Linux `DRM_CLIENT_CAP_*` value.
    ///
    /// Linux keeps this on `drm_file`, i.e. per open file description, and so
    /// does this driver: two clients of the same card must not share the
    /// `atomic` / `universal_planes` bits they negotiated independently.
    ///
    /// Only `DRM_CLIENT_CAP_ATOMIC` is read back, by `DrmModeAtomic`, which
    /// answers `EOPNOTSUPP` until the client has negotiated it.  The other
    /// accepted bits are recorded because clients branch on the answer to the
    /// setter, not because anything here acts on them: plane enumeration is the
    /// same with or without `DRM_CLIENT_CAP_UNIVERSAL_PLANES`.
    client_caps: AtomicU64,
    magic: Mutex<Option<u32>>,
}

impl Drop for DrmFile {
    fn drop(&mut self) {
        // Also rolls back ownership if the file builder is never published.
        let mut masters = self.masters.lock();
        let released_master = masters
            .active
            .as_ref()
            .is_some_and(|active| core::ptr::eq(active.owner.as_ptr(), self))
            .then_some(())
            .and_then(|()| masters.active.take().map(|active| active.master));
        if let Some(master) = &released_master {
            invalidate_magic_tokens(master);
        }
        drop(masters);

        let binding = self.binding.lock();
        if let Some(magic) = *self.magic.lock() {
            binding.master.magic_registry.lock().files.remove(&magic);
        }
    }
}

/// One DRM primary character device bound to one display backend.
pub struct DrmCard {
    display: ClassDevice<DisplayDeviceImpl>,
    masters: Arc<Mutex<MasterState>>,
    events: Mutex<VecDeque<DrmEventVblank>>,
    poll_rx: PollSet,
    sequence: AtomicU32,
    state: Mutex<ModesetState>,
    /// Serializes "publish the state, then show it" across the property entry
    /// points.
    ///
    /// `MODE_ATOMIC` and `set_one_property` both end with the same two steps:
    /// store the committed `ModesetState`, then present its `plane_fb_id`.
    /// Presenting after releasing `state` keeps the display call out of that
    /// lock, but it also lets two committers interleave so that the older
    /// framebuffer reaches the screen last; the scanout then keeps disagreeing
    /// with what `MODE_OBJ_GETPROPERTIES` reports, and `present_fb` cannot tell
    /// anyone because an unknown framebuffer is a silent no-op.
    ///
    /// Always taken before `state`, never while holding it, so the order is
    /// `commit_lock` -> `state` -> the inner maps `apply_prop` touches.
    commit_lock: Mutex<()>,
    /// The framebuffer `present_fb` last put on screen.
    ///
    /// `legacy_crtc.fb_id` and `state.plane_fb_id` record what a client asked
    /// for, but `MODE_CRTC_PAGE_FLIP` and `MODE_DIRTYFB` present without
    /// touching either, so they cannot answer "is this framebuffer still on
    /// screen?" on their own.
    presented_fb: AtomicU32,
    /// The scanout resource `present_fb` last handed to the host, or 0.
    ///
    /// Retirement has to keep this resource alive: this driver has no "stop
    /// scanning" call, so the host keeps scanning the last resource it was
    /// given until another one is presented.  "No framebuffer references it"
    /// is therefore not enough on its own.
    presented_resource: AtomicU32,
    /// Resources whose scanout state is unknown.
    ///
    /// `present_scanout_resource()` issues `TRANSFER_TO_HOST_2D`, conditionally
    /// issues `SET_SCANOUT` when the active resource or rectangle changes, and
    /// issues `RESOURCE_FLUSH`. When one of the commands fails, there is no way
    /// to tell how far the host got, so the new resource joins this set and the
    /// previous scanout target is still in `presented_resource`; both are held
    /// back from retirement. A set rather than a slot, because several failed
    /// presents can pile up before one succeeds. The next successful present
    /// settles all of them at once: this driver has a single scanout slot
    /// (`SET_SCANOUT` replaces the target), so nothing in the set can still be on
    /// screen afterwards.
    unsettled: Mutex<BTreeSet<u32>>,
    /// Dumb buffers whose handle was closed while a registered framebuffer
    /// still named their scanout resource.
    ///
    /// A framebuffer only copies the resource id out of the dumb buffer, so the
    /// resource and its backing have to outlive the handle.  Parked here and
    /// destroyed by the last framebuffer that references them (`RMFB` /
    /// `MODE_CLOSEFB`), which is the ordering the GEM object refcount gives
    /// Linux for free.
    retired_dumbs: Mutex<BTreeMap<u32, DumbBuffer>>,
    legacy_crtc: Mutex<LegacyCrtcState>,
    dumbs: Mutex<BTreeMap<u32, DumbBuffer>>,
    next_dumb_handle: AtomicU32,
    next_offset: AtomicU64,
    fbs: Mutex<BTreeMap<u32, Framebuffer>>,
    next_fb_id: AtomicU32,
    next_resource_id: AtomicU32,
    blobs: Mutex<BTreeMap<u32, Arc<Vec<u8>>>>,
    mode_id_blob_ref: Mutex<Option<Arc<Vec<u8>>>>,
    next_blob_id: AtomicU32,
    system_blobs: Mutex<BTreeMap<u32, Arc<Vec<u8>>>>,
    in_formats_blob: AtomicU32,
    system_blobs_init: Mutex<()>,
    retained_pages: Mutex<BTreeMap<u64, Arc<GlobalPage>>>,
}

impl DrmCard {
    /// Create a new DRM device instance.
    pub fn new(display: ClassDevice<DisplayDeviceImpl>) -> Arc<Self> {
        Arc::new(Self {
            display,
            masters: Arc::new(Mutex::new(MasterState::default())),
            events: Mutex::new(VecDeque::with_capacity(MAX_EVENTS)),
            poll_rx: PollSet::new(),
            sequence: AtomicU32::new(0),
            state: Mutex::new(ModesetState::default()),
            commit_lock: Mutex::new(()),
            presented_fb: AtomicU32::new(0),
            presented_resource: AtomicU32::new(0),
            unsettled: Mutex::new(BTreeSet::new()),
            retired_dumbs: Mutex::new(BTreeMap::new()),
            legacy_crtc: Mutex::new(LegacyCrtcState::default()),
            dumbs: Mutex::new(BTreeMap::new()),
            next_dumb_handle: AtomicU32::new(FIRST_DUMB_HANDLE),
            next_offset: AtomicU64::new(DUMB_BUFFER_OFFSET_STRIDE),
            fbs: Mutex::new(BTreeMap::new()),
            next_fb_id: AtomicU32::new(FIRST_FB_ID),
            next_resource_id: AtomicU32::new(0x1000),
            blobs: Mutex::new(BTreeMap::new()),
            mode_id_blob_ref: Mutex::new(None),
            next_blob_id: AtomicU32::new(FIRST_BLOB_ID),
            system_blobs: Mutex::new(BTreeMap::new()),
            in_formats_blob: AtomicU32::new(0),
            system_blobs_init: Mutex::new(()),
            retained_pages: Mutex::new(BTreeMap::new()),
        })
    }

    /// Wake poll waiters after the bound display is removed.
    pub fn notify_removed(&self) {
        self.poll_rx.wake();
    }

    fn display(&self) -> VfsResult<&ClassDevice<DisplayDeviceImpl>> {
        self.display
            .is_available()
            .then_some(&self.display)
            .ok_or(VfsError::NoSuchDevice)
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

fn display_resolution(dev: &DrmCard) -> (u32, u32) {
    dev.display()
        .map(|display| {
            let info = display.info();
            (info.width, info.height)
        })
        .unwrap_or((640, 480))
}

fn current_mode(dev: &DrmCard) -> DrmModeModeInfo {
    let (w, h) = display_resolution(dev);
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

impl DeviceFileOps for DrmCard {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        self.display()?;
        let mut masters = self.masters.lock();
        if masters
            .active
            .as_ref()
            .is_some_and(|active| active.owner.strong_count() == 0)
        {
            let Some(released_master) = masters.active.take().map(|active| active.master) else {
                return Err(VfsError::InvalidInput);
            };
            invalidate_magic_tokens(&released_master);
        }
        let is_master = masters.active.is_none();
        let master = match &masters.active {
            Some(active) => active.master.clone(),
            None => Arc::new(DrmMaster::default()),
        };
        let state = Arc::new(DrmFile {
            masters: self.masters.clone(),
            binding: Mutex::new(MasterBinding {
                master: master.clone(),
                is_master,
            }),
            client_caps: AtomicU64::new(0),
            magic: Mutex::new(None),
        });
        if is_master {
            masters.active = Some(ActiveMaster {
                master,
                owner: Arc::downgrade(&state),
            });
        }
        file.set_private_data(state);
        drop(masters);
        Ok(())
    }

    fn release(&self, _inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        let mut masters = self.masters.lock();
        let released_master = masters
            .active
            .as_ref()
            .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(&state)))
            .then_some(())
            .and_then(|()| masters.active.take().map(|active| active.master));
        if let Some(master) = &released_master {
            invalidate_magic_tokens(master);
        }
        drop(masters);
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        self.display()?;
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

    #[expect(
        clippy::too_many_lines,
        reason = "CG-028-focused-functions: linear DRM ioctl dispatch table: one short match arm \
                  per command, arms share no state"
    )]
    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        self.display()?;
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        match cmd {
            DRM_IOCTL_SET_MASTER => {
                let mut masters = self.masters.lock();
                if let Some(active) = &masters.active {
                    return if Weak::ptr_eq(&active.owner, &Arc::downgrade(&state)) {
                        Ok(0)
                    } else {
                        Err(VfsError::ResourceBusy)
                    };
                }
                let mut binding = state.binding.lock();
                if !binding.is_master {
                    let mut file_magic = state.magic.lock();
                    if let Some(magic) = file_magic.take() {
                        binding.master.magic_registry.lock().files.remove(&magic);
                    }
                    binding.master = Arc::new(DrmMaster::default());
                    binding.is_master = true;
                }
                masters.active = Some(ActiveMaster {
                    master: binding.master.clone(),
                    owner: Arc::downgrade(&state),
                });
                Ok(0)
            }
            DRM_IOCTL_DROP_MASTER => {
                let mut masters = self.masters.lock();
                if !masters
                    .active
                    .as_ref()
                    .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(&state)))
                {
                    return Err(VfsError::InvalidInput);
                }
                let Some(released_master) = masters.active.take().map(|active| active.master)
                else {
                    return Err(VfsError::InvalidInput);
                };
                invalidate_magic_tokens(&released_master);
                drop(masters);
                Ok(0)
            }
            <DrmVersion as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmVersion>::from(arg);
                <DrmVersion as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmUnique as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmUnique>::from(arg);
                <DrmUnique as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmSetVersion as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmSetVersion>::from(arg);
                <DrmSetVersion as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmGetCap as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmGetCap>::from(arg);
                <DrmGetCap as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmSetClientCap as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmSetClientCap>::from(arg);
                <DrmSetClientCap as DrmIoctl>::handle_raw_input(self, &state, user_ptr)
            }
            <DrmAuth as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmAuth>::from(arg);
                <DrmAuth as DrmIoctl>::handle_raw_output(self, &state, user_ptr)
            }
            <DrmAuthMagic as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmAuthMagic>::from(arg);
                <DrmAuthMagic as DrmIoctl>::handle_raw_input(self, &state, user_ptr)
            }
            <DrmModeDirtyFb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeDirtyFb>::from(arg);
                <DrmModeDirtyFb as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmPrimeHandle as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmPrimeHandle>::from(arg);
                <DrmPrimeHandle as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmPrimeFdToHandle as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmPrimeFdToHandle>::from(arg);
                <DrmPrimeFdToHandle as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeCardRes as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCardRes>::from(arg);
                <DrmModeCardRes as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeCrtc as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCrtc>::from(arg);
                <DrmModeCrtc as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeSetCrtc as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeSetCrtc>::from(arg);
                <DrmModeSetCrtc as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeGetEncoder as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetEncoder>::from(arg);
                <DrmModeGetEncoder as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeGetConnector as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetConnector>::from(arg);
                <DrmModeGetConnector as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeRmFb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeRmFb>::from(arg);
                <DrmModeRmFb as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeCrtcPageFlip as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCrtcPageFlip>::from(arg);
                <DrmModeCrtcPageFlip as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeCreateDumb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCreateDumb>::from(arg);
                <DrmModeCreateDumb as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeMapDumb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeMapDumb>::from(arg);
                <DrmModeMapDumb as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeDestroyDumb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeDestroyDumb>::from(arg);
                <DrmModeDestroyDumb as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmGemClose as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmGemClose>::from(arg);
                <DrmGemClose as DrmIoctl>::handle_raw_input(self, &state, user_ptr)
            }
            <DrmModeCloseFb as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCloseFb>::from(arg);
                <DrmModeCloseFb as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeGetPlaneRes as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetPlaneRes>::from(arg);
                <DrmModeGetPlaneRes as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeGetPlane as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetPlane>::from(arg);
                <DrmModeGetPlane as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeObjGetProperties as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeObjGetProperties>::from(arg);
                <DrmModeObjGetProperties as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeConnectorSetProperty as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeConnectorSetProperty>::from(arg);
                <DrmModeConnectorSetProperty as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeObjSetProperty as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeObjSetProperty>::from(arg);
                <DrmModeObjSetProperty as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeGetProperty as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetProperty>::from(arg);
                <DrmModeGetProperty as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmWaitVblank as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmWaitVblank>::from(arg);
                <DrmWaitVblank as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeAtomic as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeAtomic>::from(arg);
                <DrmModeAtomic as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeCreateBlob as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCreateBlob>::from(arg);
                <DrmModeCreateBlob as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeDestroyBlob as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeDestroyBlob>::from(arg);
                <DrmModeDestroyBlob as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeGetBlob as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeGetBlob>::from(arg);
                <DrmModeGetBlob as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeCursor as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCursor>::from(arg);
                <DrmModeCursor as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeCursor2 as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeCursor2>::from(arg);
                <DrmModeCursor2 as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeFbCmd as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeFbCmd>::from(arg);
                <DrmModeFbCmd as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            <DrmModeFbCmd2 as DrmIoctl>::CMD => {
                let user_ptr = UserPtr::<DrmModeFbCmd2>::from(arg);
                <DrmModeFbCmd2 as DrmIoctl>::handle_raw(self, &state, user_ptr)
            }
            _ => Err(kvfs::VfsError::OperationNotSupported),
        }
    }

    fn mmap(&self, _file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        self.display()?;
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

impl Pollable for DrmCard {
    fn poll(&self) -> IoEvents {
        if self.display().is_err() {
            return IoEvents::ERR | IoEvents::HUP;
        }
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

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, crtc: &mut Self) -> VfsResult<usize> {
        let c = &crtc.0;
        if c.crtc_id != CRTC_ID {
            return Err(VfsError::InvalidInput);
        }
        // `MODE_CLOSEFB` removes a framebuffer while holding this lock, so the
        // binding written below cannot be outrun by a concurrent close.
        let _commit = dev.commit_lock.lock();
        if c.fb_id == 0 && c.count_connectors == 0 {
            *dev.legacy_crtc.lock() = LegacyCrtcState::default();
            // The CRTC no longer drives any framebuffer, so the framebuffer that
            // was last presented is not in use any more and must be closeable.
            // The resource marker stays: the host keeps scanning it until
            // something else is presented.
            dev.presented_fb.store(0, Ordering::Relaxed);
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

    fn handle(_dev: &DrmCard, _file: &Arc<DrmFile>, e: &mut Self) -> VfsResult<usize> {
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

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, c: &mut Self) -> VfsResult<usize> {
        if c.connector_id != CONNECTOR_ID {
            return Err(VfsError::InvalidInput);
        }
        c.encoder_id = ENCODER_ID;
        c.connector_type = DRM_MODE_CONNECTOR_VIRTUAL;
        c.connector_type_id = 1;
        c.connection = DRM_MODE_CONNECTED;
        let (w, h) = display_resolution(dev);
        c.mm_width = w;
        c.mm_height = h;
        c.subpixel = 0;
        c.count_encoders = report_user_array(c.encoders_ptr, c.count_encoders, &[ENCODER_ID])?;
        if !c.modes_ptr.is_null() && c.count_modes > 0 {
            c.modes_ptr
                .write_vm(current_mode(dev))
                .map_err(|_| VfsError::BadAddress)?;
        }
        c.count_modes = 1;
        c.count_props = 0;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeRmFb {
    const CMD: u32 = iowr::<u32>(DRM_TYPE, 0xAF);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, fb_id: &mut Self) -> VfsResult<usize> {
        // Same lock as `MODE_CLOSEFB`: the retirement below reads the scanout
        // markers, which a concurrent present updates under this lock.
        let _commit = dev.commit_lock.lock();
        let removed = dev.fbs.lock().remove(&fb_id.0);
        {
            let mut legacy = dev.legacy_crtc.lock();
            if legacy.fb_id == fb_id.0 {
                *legacy = LegacyCrtcState::default();
            }
        }
        // This may have been the last framebuffer naming a dumb buffer whose
        // handle was closed earlier; that is when its resource is destroyed.
        if let Some(fb) = removed {
            retire_unused_dumb(dev, fb.resource_id);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCrtcPageFlip {
    const CMD: u32 = iowr::<DrmModeCrtcPageFlip>(DRM_TYPE, 0xB0);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, f: &mut Self) -> VfsResult<usize> {
        // `MODE_CLOSEFB` reads the presented markers under this lock, so
        // presenting and closing cannot interleave.
        let _commit = dev.commit_lock.lock();
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

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, c: &mut Self) -> VfsResult<usize> {
        // The backing store is always BGRA8888 (32bpp) and `bytes_per_pixel` is
        // therefore constant, but 24bpp requests must still be accepted: Linux's
        // generic dumb-buffer ioctl accepts them, and libdrm-based clients on
        // the non-atomic path (Xorg's modesetting driver) ask for 24bpp and then
        // hand that same value back to `ADDFB`. Rejecting it fails their
        // initialisation before any framebuffer exists.
        //
        // `bpp == 0`, `bpp > 64` and the multiple-of-8 rule are still enforced,
        // because the depth comes from user space and an unknown depth cannot be
        // described to the scanout device.
        if c.width == 0 || c.height == 0 || c.flags != 0 || !matches!(c.bpp, 24 | 32) {
            return Err(VfsError::InvalidInput);
        }
        if c.width > 16384 || c.height > 16384 {
            return Err(VfsError::InvalidInput);
        }
        let bytes_per_pixel = 4;
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
        let paddr = v2p(backing.start_va()).as_usize() as u64;
        let resource_id = dev.next_resource_id.fetch_add(1, Ordering::Relaxed);
        dev.display()?
            .create_scanout_resource(
                ScanoutResource {
                    id: resource_id,
                    width: c.width,
                    height: c.height,
                    pitch: c.pitch,
                    format: ScanoutFormat::Bgra8888,
                },
                paddr,
                size_aligned as u32,
            )
            .map_err(|_| VfsError::Io)?;
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
                pitch: c.pitch,
                offset,
                resource_id,
                pages: pages_arc,
            },
        );
        c.handle = handle;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeMapDumb {
    const CMD: u32 = iowr::<DrmModeMapDumb>(DRM_TYPE, 0xB3);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, m: &mut Self) -> VfsResult<usize> {
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

/// Drop a dumb buffer's handle and release the host scanout resource behind it.
///
/// `DESTROY_DUMB` and `GEM_CLOSE` converge on the same state in Linux --
/// `drm_mode_destroy_dumb_ioctl()` and `drm_gem_close_ioctl()` both end in
/// `drm_gem_handle_delete()` -- so they share this path instead of keeping two
/// copies of the teardown.  Returns `false` when the handle was never allocated;
/// the two callers report that case differently, which is the one place where
/// the legacy and the GEM spelling disagree.
fn take_dumb(dev: &DrmCard, handle: u32) -> VfsResult<bool> {
    // One critical section owns both the existence test and the removal, so two
    // concurrent closes of the same handle cannot both reach the teardown.
    let Some(dumb) = dev.dumbs.lock().remove(&handle) else {
        return Ok(false);
    };
    // Resolve the display device before anything becomes visible: if it is gone,
    // fail the ioctl instead of dropping the backing while the host still holds a
    // scanout resource for it (dangling backing + leak).  The lookup happens after
    // the handle test so that an unknown handle stays a silent miss -- the legacy
    // `DESTROY_DUMB` contract -- even when the device has already been removed.
    let device = match dev.display() {
        Ok(device) => device,
        Err(err) => {
            dev.dumbs.lock().insert(handle, dumb);
            return Err(err);
        }
    };
    // A registered framebuffer only copies `resource_id` out of the dumb buffer
    // and keeps no reference to the resource or its pages, so destroying them
    // here would leave every framebuffer that still names the resource pointing
    // at a detached scanout and freed backing -- and one of them may be on
    // screen.  Park the buffer instead and let the last framebuffer that
    // references it retire the resource; Linux gets this ordering from the GEM
    // object refcount.
    park_or_destroy(dev, device, dumb);
    Ok(true)
}

/// Is `resource_id` on the scanout, or one whose fate a failed present left open?
fn resource_on_scanout(dev: &DrmCard, resource_id: u32) -> bool {
    dev.presented_resource.load(Ordering::Relaxed) == resource_id
        || dev.unsettled.lock().contains(&resource_id)
}

/// Park `dumb` while anything still needs its resource, or destroy it.
///
/// Two things keep the resource alive: a registered framebuffer that copied its
/// id out, and the host scanning it -- `present_fb` leaves the host pointed at
/// the last resource it was given and this driver has no call that stops that.
/// Parked buffers are retired by `sweep_retired_dumbs()` once another resource
/// is presented.
///
/// The reference test and the insertion into the park table happen under the same
/// `fbs` lock -- the order `retire_unused_dumb()` takes them in.  Without that, a
/// close of the last framebuffer could slip in between the test and the
/// insertion, and the parked entry would never be retired.
fn park_or_destroy(dev: &DrmCard, device: &ClassDevice<DisplayDeviceImpl>, dumb: DumbBuffer) {
    {
        let fbs = dev.fbs.lock();
        let referenced = fbs.values().any(|fb| fb.resource_id == dumb.resource_id);
        let scanned = resource_on_scanout(dev, dumb.resource_id);
        if referenced || scanned {
            dev.retired_dumbs.lock().insert(dumb.resource_id, dumb);
            return;
        }
    }
    destroy_dumb(device, dumb);
}

/// Detach the scanout resource behind `dumb` and release its backing pages.
fn destroy_dumb(device: &ClassDevice<DisplayDeviceImpl>, dumb: DumbBuffer) {
    if let Err(err) = device.destroy_scanout_resource(dumb.resource_id) {
        log::warn!(
            "drm: failed to destroy scanout resource {}: {:?}",
            dumb.resource_id,
            err
        );
    }
    // `dumb` (and the backing pages it owns) is released here.
}

/// Retire a parked dumb buffer once no framebuffer references its resource.
///
/// Called by the framebuffer teardown paths, which are the only places that can
/// take the last reference away.
fn retire_unused_dumb(dev: &DrmCard, resource_id: u32) {
    // The host may still be scanning this resource even when no framebuffer
    // names it any more: `RMFB` drops the framebuffer without stopping the
    // scanout, and this driver has no call that would.  Such a buffer is left
    // parked and retired by the next present of a different resource.
    if resource_on_scanout(dev, resource_id) {
        return;
    }
    // Same lock order as `park_or_destroy()`: `fbs` first, then the park table,
    // so a concurrent park cannot be missed by this take.
    let parked = {
        let fbs = dev.fbs.lock();
        if fbs.values().any(|fb| fb.resource_id == resource_id) {
            return;
        }
        dev.retired_dumbs.lock().remove(&resource_id)
    };
    let Some(dumb) = parked else {
        return;
    };
    // The display may already be gone; the resource went with it, and only the
    // backing pages are left to release.
    if let Ok(device) = dev.display() {
        destroy_dumb(device, dumb);
    }
}

impl DrmIoctl for DrmModeDestroyDumb {
    const CMD: u32 = iowr::<DrmModeDestroyDumb>(DRM_TYPE, 0xB4);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, d: &mut Self) -> VfsResult<usize> {
        // Legacy entry point: an unknown handle stays a silent success, because
        // callers free a dumb buffer without ever reading the result.  GEM_CLOSE
        // below is the entry point that is allowed to report the miss.
        //
        // `ADDFB2` holds this lock from reading a handle to registering the
        // framebuffer, which is what keeps a close from destroying a resource
        // the new framebuffer is about to name.
        let _commit = dev.commit_lock.lock();
        take_dumb(dev, d.handle)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmGemClose {
    // DRM_IOCTL_GEM_CLOSE = DRM_IOW(0x09, struct drm_gem_close): 8 bytes.
    const CMD: u32 = iow::<DrmGemClose>(DRM_TYPE, 0x09);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, args: &mut Self) -> VfsResult<usize> {
        // `drm_gem_close_ioctl()` rejects a non-zero `pad` and reports an unknown
        // handle as EINVAL (through `drm_gem_handle_delete()`), not ENOENT.
        if args.pad != 0 {
            return Err(VfsError::InvalidInput);
        }
        // See `MODE_DESTROY_DUMB`: this lock orders the close against `ADDFB2`.
        let _commit = dev.commit_lock.lock();
        if !take_dumb(dev, args.handle)? {
            return Err(VfsError::InvalidInput);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCloseFb {
    // DRM_IOCTL_MODE_CLOSEFB = DRM_IOWR(0xD0, struct drm_mode_closefb): 8 bytes.
    const CMD: u32 = iowr::<DrmModeCloseFb>(DRM_TYPE, 0xD0);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, args: &mut Self) -> VfsResult<usize> {
        // `drm_mode_closefb_ioctl()`: a non-zero `pad` is EINVAL, an unknown
        // framebuffer is ENOENT, and a framebuffer that is still on screen is
        // EBUSY.  That in-use check is the whole difference from RMFB (0xAF),
        // which this driver implements as an unconditional removal: CLOSEFB
        // tells a client its framebuffer is still on screen instead of pulling
        // the scanout out from under it.
        //
        // "On screen" has three sources: the legacy CRTC binding, the plane
        // property committed through `MODE_ATOMIC` / `set_one_property`, and
        // whatever `present_fb` last put up -- `MODE_CRTC_PAGE_FLIP` and
        // `MODE_DIRTYFB` present without touching either of the first two.
        if args.pad != 0 {
            return Err(VfsError::InvalidInput);
        }
        // `MODE_SETCRTC` and the commit path take this lock too, so the binding
        // cannot change between the checks below and the removal.
        let _commit = dev.commit_lock.lock();
        let Some(resource_id) = dev.fbs.lock().get(&args.fb_id).map(|fb| fb.resource_id) else {
            return Err(VfsError::NotFound);
        };
        let legacy_bound = dev.legacy_crtc.lock().fb_id == args.fb_id;
        let plane_bound = dev.state.lock().plane_fb_id == args.fb_id;
        let presented = dev.presented_fb.load(Ordering::Relaxed) == args.fb_id;
        if legacy_bound || plane_bound || presented {
            return Err(VfsError::ResourceBusy);
        }
        dev.fbs.lock().remove(&args.fb_id);
        // May have been the last framebuffer naming a parked dumb buffer.
        retire_unused_dumb(dev, resource_id);
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetPlaneRes {
    const CMD: u32 = iowr::<DrmModeGetPlaneRes>(DRM_TYPE, 0xB5);

    fn handle(_dev: &DrmCard, _file: &Arc<DrmFile>, r: &mut Self) -> VfsResult<usize> {
        let planes: &[u32] = &[PLANE_ID];
        r.count_planes = report_user_array(r.plane_id_ptr, r.count_planes, planes)?;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeGetPlane {
    const CMD: u32 = iowr::<DrmModeGetPlane>(DRM_TYPE, 0xB6);

    fn handle(_dev: &DrmCard, _file: &Arc<DrmFile>, p: &mut Self) -> VfsResult<usize> {
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

impl DrmIoctl for DrmModeConnectorSetProperty {
    // DRM_IOCTL_MODE_SETPROPERTY = DRM_IOWR(0xAB, struct
    // drm_mode_connector_set_property): a 16-byte write-read.
    const CMD: u32 = iowr::<DrmModeConnectorSetProperty>(DRM_TYPE, 0xAB);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, req: &mut Self) -> VfsResult<usize> {
        // `drm_connector_property_set_ioctl()` is only a wrapper over
        // `drm_mode_obj_set_property_ioctl()` with `obj_type` fixed to
        // `DRM_MODE_OBJECT_CONNECTOR`, so the connector is the only target the
        // request can name, and the write below is the one the object form
        // performs.
        if req.connector_id != CONNECTOR_ID {
            return Err(VfsError::NotFound);
        }
        set_one_property(
            dev,
            DRM_MODE_OBJECT_CONNECTOR,
            req.connector_id,
            req.prop_id,
            req.value,
        )
    }
}

impl DrmIoctl for DrmModeObjSetProperty {
    // DRM_IOCTL_MODE_OBJ_SETPROPERTY = DRM_IOWR(0xBA, struct
    // drm_mode_obj_set_property): 24 bytes after alignment, not 20.
    const CMD: u32 = iowr::<DrmModeObjSetProperty>(DRM_TYPE, 0xBA);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, req: &mut Self) -> VfsResult<usize> {
        // The object type travels in the request, so it has to agree with the id.
        // This is the check MODE_ATOMIC applies to every entry of its object list,
        // and it is what stops a client from writing a plane property onto the CRTC.
        if object_type_of(req.obj_id) != Some(req.obj_type) {
            return Err(VfsError::NotFound);
        }
        set_one_property(dev, req.obj_type, req.obj_id, req.prop_id, req.value)
    }
}

/// Write one property from a non-atomic entry point.
///
/// The sequence is a copy of the one `MODE_ATOMIC` performs inline: apply to a
/// copy of the state, commit the copy only if the write succeeded, publish a new
/// mode blob if the write produced one, and refresh the scanout whenever the
/// committed plane framebuffer is non-zero.  `MODE_ATOMIC` is deliberately left
/// alone here: it carries `TEST_ONLY` and page-flip-event handling that this
/// helper has no business absorbing.  Both here and there the commit is
/// serialized by `commit_lock`, so a second committer cannot slip its own
/// framebuffer in between the store and the present.
///
/// Errnos follow Linux's `drm_mode_obj_set_property_ioctl()`, whose `ret` starts
/// at `-EINVAL`: a request that names an unknown object, or an object type that
/// does not match the id, fails the `drm_mode_object_find()` lookup and answers
/// `ENOENT`; a known object carrying an unknown property id reaches `out_unref`
/// with `ret` untouched and answers `EINVAL`, as does an invalid value for a
/// known property.  `DRM_IOCTL_MODE_SETPROPERTY` only wraps that function with
/// `obj_type = DRM_MODE_OBJECT_CONNECTOR`, so these two entry points and
/// `MODE_ATOMIC` all answer alike and a client cannot tell which one it used.
fn set_one_property(
    dev: &DrmCard,
    obj_type: u32,
    obj_id: u32,
    prop_id: u32,
    value: u64,
) -> VfsResult<usize> {
    // The store below and the present after it have to stay one operation for
    // other committers, or the older framebuffer can reach the screen last.
    let _commit = dev.commit_lock.lock();
    let mut state = dev.state.lock();
    let previous_fb = state.plane_fb_id;
    let mut proposed = *state;
    let mut new_mode_blob: Option<Option<Arc<Vec<u8>>>> = None;
    if !dev.apply_prop(
        obj_type,
        obj_id,
        prop_id,
        value,
        &mut proposed,
        &mut new_mode_blob,
    )? {
        // Unknown property on a known object: Linux's
        // `drm_mode_obj_set_property_ioctl()` leaves `ret` at its `-EINVAL`
        // initialiser on this path; only a failed object lookup is `ENOENT`.
        return Err(VfsError::InvalidInput);
    }
    let current_fb = proposed.plane_fb_id;
    *state = proposed;
    // Publish the blob before releasing `state`.  `apply_prop` takes the same two
    // locks in this order, so doing the store here keeps that order; releasing
    // `state` first would let a second writer commit its own `crtc_mode_id` in
    // the gap, and `GET_BLOB` would then find a blob id that no longer matches the
    // reference and answer with another mode's timings instead of an error.
    if let Some(new_ref) = new_mode_blob {
        *dev.mode_id_blob_ref.lock() = new_ref;
    }
    drop(state);
    if current_fb != 0 {
        dev.present_fb(current_fb);
    } else if previous_fb != 0 {
        // This commit is the one that unbound the plane, so `MODE_CLOSEFB` must
        // no longer refuse the framebuffer that was up there.  The resource
        // marker stays: this driver has no call that stops the host from
        // scanning it, so it is retired by the next present of another
        // resource (`sweep_retired_dumbs`).
        dev.presented_fb.store(0, Ordering::Relaxed);
    }
    Ok(0)
}

impl DrmIoctl for DrmModeObjGetProperties {
    const CMD: u32 = iowr::<DrmModeObjGetProperties>(DRM_TYPE, 0xB9);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, q: &mut Self) -> VfsResult<usize> {
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

    fn handle(_dev: &DrmCard, _file: &Arc<DrmFile>, g: &mut Self) -> VfsResult<usize> {
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

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, request: &mut Self) -> VfsResult<usize> {
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

        const FRAME_PERIOD_NS: u64 = ktime_types::NANOS_PER_SEC / 60;
        let delay =
            ktime_types::TimeSpan::from_nanos(FRAME_PERIOD_NS.saturating_mul(wait_count as u64));
        ktask::sleep(delay);
        dev.sequence.fetch_add(wait_count, Ordering::AcqRel);

        let now = khal::time::monotonic_time();
        let now = now.span_since_origin();
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

    fn handle(dev: &DrmCard, file: &Arc<DrmFile>, a: &mut Self) -> VfsResult<usize> {
        // This is the consumer of the bitmap SET_CLIENT_CAP maintains: a client
        // that never negotiated `DRM_CLIENT_CAP_ATOMIC` is told the atomic path
        // is unavailable so it falls back to the legacy one, instead of being
        // handed an interface it did not ask for.
        if file.client_caps.load(Ordering::Relaxed) & (1u64 << DRM_CLIENT_CAP_ATOMIC) == 0 {
            return Err(VfsError::OperationNotSupported);
        }

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

        // Same ordering as `set_one_property`: one committer at a time, so the
        // commit and the present below cannot be split by another writer.
        let _commit = dev.commit_lock.lock();
        let mut state = dev.state.lock();
        let previous_fb = state.plane_fb_id;
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
        } else if previous_fb != 0 {
            // Unbinding the plane tears the scanout down; see the same branch
            // in `set_one_property` -- the resource marker deliberately stays.
            dev.presented_fb.store(0, Ordering::Relaxed);
        }
        if a.flags & DRM_MODE_PAGE_FLIP_EVENT != 0 {
            dev.queue_flip_event(a.user_data);
        }
        Ok(0)
    }
}

impl DrmIoctl for DrmModeCreateBlob {
    const CMD: u32 = iowr::<DrmModeCreateBlob>(DRM_TYPE, 0xBD);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, c: &mut Self) -> VfsResult<usize> {
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

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, d: &mut Self) -> VfsResult<usize> {
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

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, g: &mut Self) -> VfsResult<usize> {
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

impl DrmIoctl for DrmModeCursor {
    const CMD: u32 = iowr::<DrmModeCursor>(DRM_TYPE, 0xA3);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, cursor: &mut Self) -> VfsResult<usize> {
        dev.set_cursor(CursorRequest {
            crtc_id: cursor.crtc_id,
            flags: cursor.flags,
            width: cursor.width,
            height: cursor.height,
            handle: cursor.handle,
        })
    }
}

impl DrmIoctl for DrmModeCursor2 {
    const CMD: u32 = iowr::<DrmModeCursor2>(DRM_TYPE, 0xBB);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, cursor: &mut Self) -> VfsResult<usize> {
        // The hotspot is only meaningful to a hardware cursor plane, which this
        // driver does not expose; validating it would reject requests the device
        // is happy to treat as a plain move.
        dev.set_cursor(CursorRequest {
            crtc_id: cursor.crtc_id,
            flags: cursor.flags,
            width: cursor.width,
            height: cursor.height,
            handle: cursor.handle,
        })
    }
}

impl DrmIoctl for DrmModeFbCmd {
    const CMD: u32 = iowr::<DrmModeFbCmd>(DRM_TYPE, 0xAE);

    fn handle(dev: &DrmCard, file: &Arc<DrmFile>, f: &mut Self) -> VfsResult<usize> {
        // Legacy ADDFB. Translate the request into the modern form and reuse
        // that implementation, so the two entry points cannot drift apart.
        //
        // bpp 24 is accepted on purpose: `DrmModeCreateDumb` backs every request
        // with a 32bpp BGRA buffer and reports `pitch = width * 4`, and callers
        // hand that same bpp straight back here.
        //
        // Unlike ADDFB2 there is no pixel_format field; the legacy ABI implies
        // XRGB8888 for depth 24, which is what the driver implements.
        // ADDFB always registers XRGB8888 below, so only accept the depths that
        // format actually describes: 24 (RGB) and 32 (XRGB with a spare byte).
        if f.handle == 0 || !matches!(f.bpp, 24 | 32) || !matches!(f.depth, 24 | 32) {
            return Err(VfsError::InvalidInput);
        }
        let mut modern = DrmModeFbCmd2 {
            fb_id: f.fb_id,
            width: f.width,
            height: f.height,
            pixel_format: DRM_FORMAT_XRGB8888,
            flags: 0,
            handles: [f.handle, 0, 0, 0],
            pitches: [f.pitch, 0, 0, 0],
            offsets: [0; 4],
            abi_pad: 0,
            modifier: [0; 4],
        };
        <DrmModeFbCmd2 as DrmIoctl>::handle(dev, file, &mut modern)?;
        f.fb_id = modern.fb_id;
        Ok(0)
    }
}

impl DrmIoctl for DrmModeFbCmd2 {
    const CMD: u32 = iowr::<DrmModeFbCmd2>(DRM_TYPE, 0xB8);

    fn handle(dev: &DrmCard, _file: &Arc<DrmFile>, f: &mut Self) -> VfsResult<usize> {
        // The dumb handle is read here and the framebuffer is registered further
        // down; `GEM_CLOSE` / `DESTROY_DUMB` take the same lock, so the handle
        // cannot be closed (and its resource destroyed) in between.
        let _commit = dev.commit_lock.lock();
        let handle = f.handles[0];
        if handle == 0 || f.offsets[0] != 0 {
            return Err(VfsError::InvalidInput);
        }
        for i in 1..4 {
            if f.handles[i] != 0 || f.pitches[i] != 0 || f.offsets[i] != 0 {
                return Err(VfsError::InvalidInput);
            }
        }
        if !matches!(f.pixel_format, DRM_FORMAT_XRGB8888 | DRM_FORMAT_ARGB8888) {
            return Err(VfsError::InvalidInput);
        }
        let (resource_id, width, height, pitch) = {
            let dumbs = dev.dumbs.lock();
            let Some(b) = dumbs.get(&handle) else {
                return Err(VfsError::InvalidInput);
            };
            (b.resource_id, b.width, b.height, b.pitch)
        };
        // The scanout resource is created at exactly the dumb buffer's size
        // and pitch (no ANY_LAYOUT negotiation), and TRANSFER_TO_HOST_2D reads
        // the backing row-by-row with the resource pitch. A sub-size
        // framebuffer would be misread (garbled scanout) or rejected by the
        // device, so the framebuffer must match the resource layout exactly.
        if f.width != width || f.height != height || f.pitches[0] != pitch {
            return Err(VfsError::InvalidInput);
        }
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
        dev.fbs.lock().insert(
            fb_id,
            Framebuffer {
                resource_id,
                width: f.width,
                height: f.height,
            },
        );
        f.fb_id = fb_id;
        Ok(0)
    }
}

impl DrmCard {
    /// Answer a legacy `MODE_CURSOR` / `MODE_CURSOR2` request.
    ///
    /// The bound display exposes a single primary plane, so there is no hardware
    /// cursor to program.  A bare move is answered with success -- libdrm-based
    /// clients such as Xorg's modesetting driver issue it once the CRTC is up and
    /// treat a failure as an initialisation error -- but a request that installs
    /// an image is refused with `EOPNOTSUPP`.  Accepting the image would tell the
    /// client its hardware cursor works, and it would then stop drawing the
    /// pointer itself while nothing appeared on screen.
    ///
    /// Validation otherwise matches Linux's `drm_mode_cursor_common()`: the CRTC
    /// must exist, and a request that carries a handle must reference a live dumb
    /// buffer of non-zero size.  Position and hotspot are accepted and dropped,
    /// which is the same observable behaviour as a driver whose cursor plane is
    /// disabled.
    fn set_cursor(&self, request: CursorRequest) -> VfsResult<usize> {
        if request.crtc_id != CRTC_ID {
            return Err(VfsError::InvalidInput);
        }
        if request.flags & !(DRM_MODE_CURSOR_BO | DRM_MODE_CURSOR_MOVE) != 0 {
            return Err(VfsError::InvalidInput);
        }
        // A BO request carries the cursor image.  Validate it first, so a
        // malformed request is reported as such: the handle must name a buffer
        // this device allocated, and width/height are the only size the request
        // provides.  A BO with `handle == 0` hides the cursor and carries no
        // image, which Linux accepts; MOVE only relocates, so its width/height
        // are ignored.
        if request.flags & DRM_MODE_CURSOR_BO != 0 && request.handle != 0 {
            if request.width == 0
                || request.height == 0
                || !self.dumbs.lock().contains_key(&request.handle)
            {
                return Err(VfsError::InvalidInput);
            }
            // The image itself cannot be shown: there is no cursor plane, so
            // answering Ok would tell the client its hardware cursor is installed
            // and it would stop drawing the pointer while nothing appeared on
            // screen.  Refusing is what keeps the software cursor, and it is the
            // same answer this driver gave before MODE_CURSOR existed.
            return Err(VfsError::OperationNotSupported);
        }
        Ok(0)
    }

    fn present_fb(&self, fb_id: u32) {
        let (resource_id, width, height) = match self.fbs.lock().get(&fb_id) {
            Some(fb) => (fb.resource_id, fb.width, fb.height),
            None => return,
        };
        let Ok(device) = self.display() else {
            return;
        };
        let rect = ScanoutRect {
            x: 0,
            y: 0,
            width,
            height,
        };
        if let Err(err) = device.present_scanout_resource(resource_id, rect) {
            log::warn!(
                "drm: failed to present scanout resource {}: {:?}",
                resource_id,
                err
            );
            // The host may already have switched (a later command in the same
            // call can be the one that failed), so the new resource is held back
            // as well: retirement skips both markers until a present succeeds.
            self.unsettled.lock().insert(resource_id);
            return;
        }
        // Remember what is actually being scanned out: `MODE_CLOSEFB` has to
        // refuse to close a framebuffer that a page flip or a dirtyfb put up
        // there, and neither of those records itself anywhere else.  The
        // resource is recorded too, because retirement has to keep scanning
        // targets alive after their framebuffer is gone.
        self.presented_fb.store(fb_id, Ordering::Relaxed);
        self.presented_resource
            .store(resource_id, Ordering::Relaxed);
        // A successful `SET_SCANOUT` replaces the one and only scanout target, so
        // every earlier "unknown" resource is now provably off screen: the set can
        // be dropped wholesale, otherwise an entry whose framebuffer is closed
        // later would never be retired.  The resource just presented is tracked by
        // `presented_resource` from here on.
        self.unsettled.lock().clear();
        // A new scanout target is up, so parked buffers that nothing else
        // names may now be retired.
        self.sweep_retired_dumbs();
    }

    /// Retire parked dumb buffers that nothing references and nothing scans.
    ///
    /// Called after a present: that is the moment the set of resources the host
    /// is scanning changes, so it is when a parked buffer can become safe to
    /// destroy.
    fn sweep_retired_dumbs(&self) {
        // Called from every present, including the per-frame page-flip path.  A
        // parked entry can legitimately live as long as its framebuffer does (the
        // close-the-handle-first sequence), so the empty check is only the cheap
        // exit for clients that never close a handle early.
        if self.retired_dumbs.lock().is_empty() {
            return;
        }
        let presented = self.presented_resource.load(Ordering::Relaxed);
        // Lock order matches the retirement paths: `fbs` -> `retired_dumbs` ->
        // `unsettled`.  `park_or_destroy()` reads the unsettled set through
        // `resource_on_scanout()` while holding `fbs`, and never takes `fbs` from
        // inside any of them, so this nesting cannot invert.
        let victims: Vec<u32> = {
            let fbs = self.fbs.lock();
            let retired = self.retired_dumbs.lock();
            let unsettled = self.unsettled.lock();
            let referenced: BTreeSet<u32> = fbs.values().map(|fb| fb.resource_id).collect();
            retired
                .keys()
                .copied()
                .filter(|resource_id| {
                    *resource_id != presented
                        && !unsettled.contains(resource_id)
                        && !referenced.contains(resource_id)
                })
                .collect()
        };
        if victims.is_empty() {
            return;
        }
        let Ok(device) = self.display() else {
            return;
        };
        for resource_id in victims {
            // Take the entry out under the lock and destroy it outside, like the
            // other two retirement sites: `destroy_dumb` calls into the display
            // backend and drops up to 8 MiB of backing.
            let parked = self.retired_dumbs.lock().remove(&resource_id);
            if let Some(dumb) = parked {
                destroy_dumb(device, dumb);
            }
        }
    }

    fn queue_flip_event(&self, user_data: u64) {
        let seq = self
            .sequence
            .fetch_add(1, Ordering::Relaxed)
            .wrapping_add(1);
        let now = khal::time::monotonic_time();
        let now = now.span_since_origin();
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
