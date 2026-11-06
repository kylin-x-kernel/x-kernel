// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The card object and its core methods.
//!
//! `DrmDevice` owns everything that is device-wide: the backend, the object
//! registries, the committed KMS state, the scanout slot, master ownership and
//! the event queue. The struct, its construction and the device-wide accessors
//! live here, and the child modules carry the card-core methods: `commit`
//! publishes the KMS state and presents it, `events` fronts the event queue,
//! and `minor` holds `DrmMinor`, the per-cdev `DeviceFileOps` adapter. Each
//! registry's methods live with its domain instead — the blob registry in
//! `blob.rs`, the framebuffer registry in `framebuffer.rs`, master ownership
//! in `auth.rs` — as additional impl blocks on this struct, the way `auth.rs`
//! has always done.
//!
//! All state mutation that userspace can observe goes through [`DrmDevice::commit`],
//! so the state and what is on screen cannot disagree.

mod commit;
mod events;
mod minor;

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use minor::DrmMinor;

use crate::prelude::*;

/// The DRM minor an open file describes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MinorKind {
    /// Modesetting node (`/dev/dri/cardN`).
    Primary,
    /// Render-only node (`/dev/dri/renderD*`).
    Render,
}

/// Sink the node owner installs to publish a display hotplug notification.
///
/// The DRM object model does not know how a card is announced to userspace;
/// the filesystem that owns the device node installs this and turns the call
/// into whatever its platform uses (a kobject uevent in X-Kernel).
pub type HotplugSink = Arc<dyn Fn() + Send + Sync>;

/// One DRM card: both minors resolve to this object.
pub struct DrmDevice {
    backend: BackendHandle,
    /// Framebuffer registry; also touched by `framebuffer.rs`.
    pub(crate) framebuffers: Mutex<IdRegistry<Framebuffer, FbTag>>,
    /// Id -> blob lookup, holding one reference per id: a client blob lives
    /// until `DESTROY_BLOB` removes it (as Linux's idr entry does), while the
    /// committed state and the system slot hold their own references.
    ///
    /// Also touched by `blob.rs`.
    pub(crate) blobs: Mutex<IdRegistry<Arc<Blob>, BlobTag>>,
    /// Committed KMS state; written by `device/commit.rs`, unbound from by
    /// `framebuffer.rs`.
    pub(crate) state: Mutex<KmsState>,
    /// Serialises "publish state, then present".
    ///
    /// `blob.rs` and `framebuffer.rs` hold it across their registry sections
    /// too.
    pub(crate) commit: Mutex<()>,
    /// What the host is scanning; also touched by `framebuffer.rs`.
    pub(crate) scanout: Mutex<ScanoutSlot>,
    /// Master ownership; also touched by `auth.rs`.
    pub(crate) master: Mutex<MasterState>,
    events: EventQueue,
    /// The IN_FORMATS blob slot; also touched by `blob.rs`.
    pub(crate) in_formats: Mutex<Option<(BlobId, Arc<Blob>)>>,
    /// `MODE_ID` blob the driver created for a legacy `SETCRTC`.
    ///
    /// A legacy modeset builds a blob itself, so the driver — not a client —
    /// owns it. Keeping the slot is what lets the next modeset free the
    /// previous one; without it every SETCRTC leaked a blob.
    ///
    /// Also touched by `blob.rs`.
    pub(crate) legacy_mode: Mutex<Option<(BlobId, Arc<Blob>)>>,
    /// Called when a refresh observes a host display change.
    hotplug: Mutex<Option<HotplugSink>>,
    retained_pages: Mutex<BTreeMap<u64, Arc<PageRef>>>,
    next_dumb: AtomicU32,
    next_offset: AtomicU64,
    next_resource: AtomicU32,
}

impl DrmDevice {
    /// Creates a card over one display backend.
    pub fn new(display: ClassDevice<DisplayDeviceImpl>) -> Arc<Self> {
        Self::new_with_backend(Arc::new(ClassBackend::new(display)))
    }

    /// Creates a card over an arbitrary backend.
    ///
    /// The display adapter above and the unit tests below both go through here,
    /// so the object model is exercised without a display device.
    pub(crate) fn new_with_backend(backend: BackendHandle) -> Arc<Self> {
        Arc::new(Self {
            backend,
            framebuffers: Mutex::new(IdRegistry::new(FIRST_FB_ID)),
            blobs: Mutex::new(IdRegistry::new(FIRST_BLOB_ID)),
            state: Mutex::new(KmsState::default()),
            commit: Mutex::new(()),
            scanout: Mutex::new(ScanoutSlot::default()),
            master: Mutex::new(MasterState::default()),
            events: EventQueue::new(),
            in_formats: Mutex::new(None),
            legacy_mode: Mutex::new(None),
            hotplug: Mutex::new(None),
            retained_pages: Mutex::new(BTreeMap::new()),
            next_dumb: AtomicU32::new(FIRST_DUMB_HANDLE),
            next_offset: AtomicU64::new(DUMB_BUFFER_OFFSET_STRIDE),
            next_resource: AtomicU32::new(0x1000),
        })
    }

    /// Handle for one minor of this card.
    pub fn minor(self: &Arc<Self>, kind: MinorKind) -> Arc<dyn DeviceFileOps> {
        // The minor owns the card: the cdev is the last thing that goes away
        // when devfs removes the node, so the object graph cannot outlive it or
        // be collected while the node is still openable.
        Arc::new(DrmMinor::new(self.clone(), kind))
    }

    /// The backend, or `ENODEV` once the display disappeared.
    pub(crate) fn backend(&self) -> DrmResult<&BackendHandle> {
        if self.backend.is_available() {
            Ok(&self.backend)
        } else {
            Err(DrmError::NoSuchDevice)
        }
    }

    /// Visible mode of the display.
    pub(crate) fn info(&self) -> DrmResult<DisplayInfo> {
        Ok(self.backend()?.info())
    }

    /// Installs the sink called when a refresh observes a host resize.
    ///
    /// The sink runs in the caller's context (a query ioctl), so it must not
    /// block or take locks that the refresh path holds. Installing it replaces
    /// any previous sink; the card keeps working without one.
    pub fn set_hotplug_sink(&self, sink: HotplugSink) {
        *self.hotplug.lock() = Some(sink);
    }

    /// Re-reads the visible mode and returns it when the host changed it.
    ///
    /// The query paths call this so a host resize (for example a resized QEMU
    /// window) is visible to a client that asks, and the installed sink is what
    /// tells a client that it should ask.
    pub(crate) fn refresh_info(&self) -> Option<DisplayInfo> {
        let changed = self.backend().ok()?.refresh_info()?;
        // Clone out of the lock: the sink publishes to userspace and must not
        // run with the device lock held.
        let sink = self.hotplug.lock().clone();
        if let Some(sink) = sink {
            sink();
        }
        Some(changed)
    }

    /// Backend capabilities.
    pub(crate) fn caps(&self) -> BackendCaps {
        self.backend.caps()
    }

    /// Wakes poll waiters after the display was removed.
    pub fn notify_removed(&self) {
        self.events.wake();
    }

    /// Allocates the next resource id.
    pub(crate) fn next_resource_id(&self) -> ResourceId {
        ResourceId::new(self.next_resource.fetch_add(1, Ordering::Relaxed))
    }

    /// Allocates the next mmap offset for a dumb buffer.
    pub(crate) fn next_mmap_offset_bytes(&self) -> u64 {
        self.next_offset
            .fetch_add(DUMB_BUFFER_OFFSET_STRIDE, Ordering::Relaxed)
    }

    /// Allocates the next dumb-buffer handle value.
    pub(crate) fn next_dumb_handle(&self) -> DumbHandle {
        DumbHandle::from_raw(self.next_dumb.fetch_add(1, Ordering::Relaxed))
    }

    /// Keeps the pages of a mapped dumb buffer alive, keyed by its mmap offset
    /// in bytes (the key the mapping interface reports).
    pub(crate) fn retain_mapping(&self, offset_bytes: u64, pages: Arc<PageRef>) {
        self.retained_pages.lock().insert(offset_bytes, pages);
    }
}
