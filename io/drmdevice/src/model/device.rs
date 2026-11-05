// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The card object and its two minors.
//!
//! `DrmDevice` owns everything that is device-wide: the backend, the object
//! registries, the committed KMS state, the scanout slot, master ownership and
//! the event queue. `DrmMinor` is the per-cdev handle the filesystem registers;
//! it only knows the node kind and a weak card reference.
//!
//! All state mutation that userspace can observe goes through [`DrmDevice::commit`],
//! so the state and what is on screen cannot disagree.

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicU32, AtomicU64, Ordering};

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
    framebuffers: Mutex<IdRegistry<Framebuffer, FbTag>>,
    /// Id -> blob lookup, holding one reference per id: a client blob lives
    /// until `DESTROY_BLOB` removes it (as Linux's idr entry does), while the
    /// committed state and the system slot hold their own references.
    blobs: Mutex<IdRegistry<Arc<Blob>, BlobTag>>,
    state: Mutex<KmsState>,
    /// Serialises "publish state, then present".
    commit: Mutex<()>,
    scanout: Mutex<ScanoutSlot>,
    /// Master ownership; also touched by `auth.rs`.
    pub(crate) master: Mutex<MasterState>,
    events: EventQueue,
    in_formats: Mutex<Option<(BlobId, Arc<Blob>)>>,
    /// `MODE_ID` blob the driver created for a legacy `SETCRTC`.
    ///
    /// A legacy modeset builds a blob itself, so the driver — not a client —
    /// owns it. Keeping the slot is what lets the next modeset free the
    /// previous one; without it every SETCRTC leaked a blob.
    legacy_mode: Mutex<Option<(BlobId, Arc<Blob>)>>,
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
        Arc::new(DrmMinor {
            device: self.clone(),
            kind,
        })
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

    /// Committed KMS state snapshot.
    pub(crate) fn kms_state(&self) -> KmsState {
        self.state.lock().clone()
    }

    /// Publishes `next` and presents its scanout.
    ///
    /// This is the only writer of the committed state. The validate, publish
    /// and present steps happen while the commit lock is held, so two clients
    /// cannot interleave such that the older framebuffer reaches the screen
    /// last.
    pub(crate) fn commit(&self, next: KmsState) -> DrmResult<()> {
        self.commit_with(|state| {
            *state = next;
            Ok(())
        })
    }

    /// Mutates the committed state and presents the result as one operation.
    ///
    /// This is the only writer of `state` and the scanout slot: validate,
    /// publish and present happen under the commit lock, so two clients cannot
    /// interleave such that the older framebuffer reaches the screen last.
    pub(crate) fn commit_with(
        &self,
        mutate: impl FnOnce(&mut KmsState) -> DrmResult<()>,
    ) -> DrmResult<()> {
        let _commit = self.commit.lock();
        let mut next = self.state.lock().clone();
        mutate(&mut next)?;
        if !next.plane_fb_id.is_none() && !self.has_framebuffer(next.plane_fb_id) {
            return Err(DrmError::InvalidInput);
        }
        // Present first, publish second: a failed present must leave the
        // committed state describing what is actually on screen. An inactive
        // CRTC shows nothing, so nothing is presented and the host keeps
        // scanning whatever it held (the slot retains it).
        // A generic commit no longer names the driver-created legacy blob (an
        // atomic commit replaced it with a client blob, or disabled the CRTC),
        // so retire it instead of leaving the id registered forever.
        let keeps_legacy_blob = self
            .legacy_mode
            .lock()
            .as_ref()
            .is_some_and(|(id, _)| next.crtc_mode.as_ref().is_some_and(|(mid, _)| mid == id));
        let presented = if next.crtc_active != 0 {
            next.plane_fb_id
        } else {
            FbId::default()
        };
        self.present_locked(presented)?;
        if !keeps_legacy_blob {
            self.clear_legacy_mode_blob();
        }
        *self.state.lock() = next;
        Ok(())
    }

    /// Validates a state proposal without publishing it (`MODE_ATOMIC` dry run).
    pub(crate) fn validate_with(
        &self,
        mutate: impl FnOnce(&mut KmsState) -> DrmResult<()>,
    ) -> DrmResult<()> {
        let _commit = self.commit.lock();
        let mut next = self.state.lock().clone();
        mutate(&mut next)?;
        if !next.plane_fb_id.is_none() && !self.has_framebuffer(next.plane_fb_id) {
            return Err(DrmError::InvalidInput);
        }
        Ok(())
    }

    /// Removes `fb` unless it is bound or on screen (`MODE_CLOSEFB`).
    ///
    /// The ownership check, the "still in use" check and the removal happen
    /// under the commit lock, so a concurrent commit cannot bind the
    /// framebuffer between them.  A file may only close a framebuffer it
    /// created; Linux reports another file's id as `ENOENT` rather than
    /// revealing that it exists.
    pub(crate) fn close_framebuffer(&self, file: &Arc<DrmFile>, fb: FbId) -> DrmResult<()> {
        let _commit = self.commit.lock();
        if !file.fbs.lock().contains(&fb) || !self.framebuffers.lock().contains(fb) {
            return Err(DrmError::NotFound);
        }
        if self.kms_state().plane_fb_id == fb || self.is_on_scanout(fb) {
            return Err(DrmError::Busy);
        }
        self.unbind_and_remove(fb).ok_or(DrmError::NotFound)?;
        Ok(())
    }

    /// Commits a legacy modeset and adopts `mode_blob` as the new legacy blob.
    ///
    /// The adoption happens under the same commit lock as the publish: doing it
    /// afterwards would let two concurrent modesets free each other's blob and
    /// leave the committed state pointing at a freed id.
    pub(crate) fn commit_legacy_modeset(
        &self,
        next: KmsState,
        mode_blob: BlobId,
        mode: Arc<Blob>,
    ) -> DrmResult<()> {
        let _commit = self.commit.lock();
        if !next.plane_fb_id.is_none() && !self.has_framebuffer(next.plane_fb_id) {
            return Err(DrmError::InvalidInput);
        }
        let presented = if next.crtc_active != 0 {
            next.plane_fb_id
        } else {
            FbId::default()
        };
        self.present_locked(presented)?;
        *self.state.lock() = next;
        self.replace_legacy_mode_blob(mode_blob, mode);
        Ok(())
    }

    /// Pushes updated pixels of `fb` to the host without changing the binding.
    ///
    /// `fb` zero means "the framebuffer currently on screen", which is what a
    /// client that lost track of its front buffer asks for. When nothing is
    /// bound there is nothing to flush.
    pub(crate) fn flush(&self, fb: FbId) -> DrmResult<()> {
        let _commit = self.commit.lock();
        let fb = if fb.is_none() {
            self.state.lock().plane_fb_id
        } else {
            fb
        };
        if fb.is_none() {
            return Ok(());
        }
        let Some((object, width, height)) = self.framebuffer_object(fb) else {
            return Err(DrmError::NotFound);
        };
        if !self.scanout.lock().is_bound(fb) {
            // The host is not scanning this resource, so there is no host copy
            // to update; its pixels are transferred when it is next presented.
            return Ok(());
        }
        self.backend()?.flush(
            object.resource()?,
            Rect {
                x: 0,
                y: 0,
                width,
                height,
            },
        )
    }

    fn present_locked(&self, fb: FbId) -> DrmResult<()> {
        if fb.is_none() {
            // Disabling the plane does not stop the host from scanning the last
            // resource, so the slot keeps it until another present replaces it.
            self.scanout.lock().unbind();
            return Ok(());
        }
        let Some((object, width, height)) = self.framebuffer_object(fb) else {
            return Err(DrmError::NotFound);
        };
        // Resolve the backend and the resource before touching the slot: a
        // failure here must not leave the slot pointing at a resource that was
        // never presented.
        let backend = self.backend()?;
        let resource = object.resource()?;
        let rollback = object.clone();
        let previous = self.scanout.lock().install(fb, object);
        let rect = Rect {
            x: 0,
            y: 0,
            width,
            height,
        };
        match backend.present(resource, rect) {
            Ok(()) => {
                // The host now scans the new resource; the previous one (and
                // any resource from an earlier failed present) can be retired.
                ScanoutSlot::release_retained(previous);
                Ok(())
            }
            Err(error) => {
                // A failed present means the host may or may not have switched:
                // keep the previous target and the attempted resource alive.
                self.scanout.lock().restore(previous, rollback);
                Err(error)
            }
        }
    }

    /// Whether `fb` names a live framebuffer.
    pub(crate) fn has_framebuffer(&self, fb: FbId) -> bool {
        self.framebuffers.lock().contains(fb)
    }

    /// Object, width and height behind `fb`.
    pub(crate) fn framebuffer_object(&self, fb: FbId) -> Option<(Arc<GemObject>, u32, u32)> {
        self.framebuffers
            .lock()
            .get(fb)
            .map(|fb| (fb.object.clone(), fb.object.width(), fb.object.height()))
    }

    /// Registers a framebuffer and records it on the creating file.
    pub(crate) fn add_framebuffer(
        &self,
        file: &Arc<DrmFile>,
        object: Arc<GemObject>,
    ) -> DrmResult<FbId> {
        let id = self
            .framebuffers
            .lock()
            .insert(Framebuffer {
                object,
                owner: Arc::downgrade(file),
            })
            .ok_or(DrmError::NoMemory)?;
        file.fbs.lock().insert(id);
        Ok(id)
    }

    /// Removes a framebuffer owned by `file` and unbinds it from the plane.
    ///
    /// Runs under the commit lock: the ownership test, unbinding the plane,
    /// dropping the registry entry and updating the creating file are one
    /// step, so a concurrent commit cannot publish or re-present a
    /// framebuffer that is being removed.  Checking ownership before touching
    /// the scanout or committed state also means a failed `RMFB` (unknown or
    /// foreign id, including zero) leaves everything unchanged.
    /// The scanout resource stays retained until another present replaces it —
    /// the host may still be scanning it.
    pub(crate) fn remove_framebuffer(&self, file: &Arc<DrmFile>, fb: FbId) -> DrmResult<()> {
        let _commit = self.commit.lock();
        if !file.fbs.lock().contains(&fb) {
            return Err(DrmError::NotFound);
        }
        self.unbind_and_remove(fb).ok_or(DrmError::NotFound)?;
        Ok(())
    }

    /// Unbinds and removes a framebuffer; the caller holds the commit lock.
    fn unbind_and_remove(&self, fb: FbId) -> Option<()> {
        if self.scanout.lock().is_bound(fb) {
            self.scanout.lock().unbind();
        }
        {
            let mut state = self.state.lock();
            if state.plane_fb_id == fb {
                state.plane_fb_id = FbId::default();
                state.plane_crtc_id = CrtcId::default();
            }
        }
        let removed = self.framebuffers.lock().remove(fb)?;
        if let Some(owner) = removed.owner.upgrade() {
            owner.fbs.lock().remove(&fb);
        }
        Some(())
    }

    /// Whether `fb` is currently bound to the plane.
    pub(crate) fn is_on_scanout(&self, fb: FbId) -> bool {
        self.scanout.lock().is_bound(fb)
    }

    /// Whether `file` currently owns the card.
    ///
    /// The device is the single authority for this question: the per-file
    /// authentication state only remembers which master object the file is
    /// bound to (for re-acquisition), never whether it may issue master-only
    /// commands.
    pub(crate) fn is_master(&self, file: &Arc<DrmFile>) -> bool {
        self.master
            .lock()
            .active
            .as_ref()
            .is_some_and(|active| Weak::ptr_eq(&active.owner, &Arc::downgrade(file)))
    }

    /// Creates the per-open file state for one minor.
    ///
    /// Shared by the `DeviceFileOps::open` adapter and the unit tests, so the
    /// master/mastership rules are exercised exactly as production does.
    pub(crate) fn open_file(self: &Arc<Self>, kind: MinorKind) -> DrmResult<Arc<DrmFile>> {
        // Opening a node whose display disappeared must fail with ENODEV, and
        // must not hand out mastership of a dead card.
        self.backend()?;
        // A render client is never master and must not inherit the card's
        // identity: it gets a private master so GET_UNIQUE answers "no
        // identity" and SET_VERSION/AUTH_MAGIC cannot touch the shared one.
        let state = if kind == MinorKind::Render {
            DrmFile::new(
                self.clone(),
                kind,
                AuthState {
                    master: Arc::new(DrmMaster::default()),
                    owns_master_object: false,
                    magic: None,
                },
            )
        } else {
            // Ownership is decided and bound while the master lock is held, so
            // two concurrent opens cannot both believe they became master.
            let mut masters = self.master.lock();
            if masters
                .active
                .as_ref()
                .is_some_and(|active| active.owner.strong_count() == 0)
                && let Some(released) = masters.active.take()
            {
                invalidate_magic_tokens(&released.master);
            }
            match &masters.active {
                Some(active) => {
                    let master = active.master.clone();
                    drop(masters);
                    DrmFile::new(
                        self.clone(),
                        kind,
                        AuthState {
                            master,
                            owns_master_object: false,
                            magic: None,
                        },
                    )
                }
                None => {
                    let master = Arc::new(DrmMaster::default());
                    let state = DrmFile::new(
                        self.clone(),
                        kind,
                        AuthState {
                            master: master.clone(),
                            owns_master_object: true,
                            magic: None,
                        },
                    );
                    masters.active = Some(ActiveMaster {
                        master,
                        owner: Arc::downgrade(&state),
                    });
                    drop(masters);
                    state
                }
            }
        };
        Ok(state)
    }

    /// Registers a blob; system blobs cannot be destroyed by clients.
    ///
    /// `creator` records the blob on the creating file so `release()` can
    /// destroy it (`drm_property_destroy_user_blobs()` in Linux).
    pub(crate) fn create_blob(
        &self,
        creator: Option<&DrmFile>,
        bytes: Arc<Vec<u8>>,
        system: bool,
    ) -> DrmResult<(BlobId, Arc<Blob>)> {
        let blob = Arc::new(Blob { bytes, system });
        let id = self
            .blobs
            .lock()
            .insert(blob.clone())
            .ok_or(DrmError::NoMemory)?;
        if let Some(file) = creator {
            file.blobs.lock().insert(id, blob.clone());
        }
        Ok((id, blob))
    }

    /// Bytes of one blob.
    pub(crate) fn blob_bytes(&self, id: BlobId) -> Option<Arc<Vec<u8>>> {
        self.blob(id).map(|blob| blob.bytes())
    }

    /// The blob behind `id`, if the id is still registered.
    pub(crate) fn blob(&self, id: BlobId) -> Option<Arc<Blob>> {
        self.blobs.lock().get(id).cloned()
    }

    /// Removes one blob.
    pub(crate) fn destroy_blob(&self, id: BlobId) -> Option<()> {
        self.blobs.lock().remove(id).map(|_| ())
    }

    /// Destroys a client blob (`MODE_DESTROY_BLOB`) owned by `creator`.
    ///
    /// A caller that names an existing blob missing from its own set gets
    /// `EPERM`, like Linux's `drm_mode_destroyblob_ioctl()`; a missing id is
    /// `ENOENT`. Driver-owned blobs are refused as well, and a stale file entry
    /// whose registry id already retired is dropped before `ENOENT` is returned.
    ///
    /// The ownership check, legacy-slot clear, device-registry removal and the
    /// creating file's bookkeeping run under one commit lock. Splitting them
    /// would leave a window where the id is gone from the device but still owned
    /// by the file, or vice versa. As in Linux, the committed state keeps its
    /// decoded mode, so only the id disappears.
    ///
    /// `creator` is the file that registered the blob; `release` passes it so
    /// close also empties the per-file set.
    pub(crate) fn destroy_client_blob(
        &self,
        creator: Option<&DrmFile>,
        id: BlobId,
    ) -> DrmResult<()> {
        let _commit = self.commit.lock();
        self.destroy_client_blob_locked(creator, id)
    }

    /// Body of [`Self::destroy_client_blob`] for callers that already hold
    /// `commit`.
    fn destroy_client_blob_locked(&self, creator: Option<&DrmFile>, id: BlobId) -> DrmResult<()> {
        let Some(blob) = self.blob(id) else {
            // A legacy-slot retirement can remove the device entry while the
            // creating file still names it; release must clear that stale entry.
            if let Some(file) = creator {
                file.blobs.lock().remove(&id);
            }
            return Err(DrmError::NotFound);
        };
        if let Some(file) = creator
            && !file.blobs.lock().contains_key(&id)
        {
            return Err(DrmError::NotPermitted);
        }
        if blob.is_system() {
            return Err(DrmError::NotPermitted);
        }
        {
            let mut legacy = self.legacy_mode.lock();
            if legacy
                .as_ref()
                .is_some_and(|(legacy_id, _)| *legacy_id == id)
            {
                legacy.take();
            }
        }
        let removed = self.destroy_blob(id).is_some();
        // Clear the file registration even if the device entry was already
        // gone (for example a legacy mode retired it); the set must not keep a
        // stale id alive until the file itself drops.
        if let Some(file) = creator {
            file.blobs.lock().remove(&id);
        }
        if !removed {
            return Err(DrmError::NotFound);
        }
        Ok(())
    }

    /// Destroys every blob created by `file` in one commit section.
    ///
    /// This is `drm_property_destroy_user_blobs()` on close: the file's set is
    /// snapshotted under `commit`, then each id is removed from the device
    /// registry and the file set by [`Self::destroy_client_blob_locked`].
    pub(crate) fn destroy_file_blobs(&self, file: &DrmFile) {
        let _commit = self.commit.lock();
        let owned: Vec<BlobId> = file.blobs.lock().keys().copied().collect();
        for id in owned {
            let _ = self.destroy_client_blob_locked(Some(file), id);
        }
    }

    /// The IN_FORMATS blob, created on first use.
    pub(crate) fn ensure_in_formats_blob(&self) -> DrmResult<BlobId> {
        let mut cached = self.in_formats.lock();
        if let Some((id, _)) = cached.as_ref() {
            return Ok(*id);
        }
        let (id, blob) = self.create_blob(None, Arc::new(build_in_formats_blob()), true)?;
        *cached = Some((id, blob));
        Ok(id)
    }

    /// Retires the legacy-mode blob after a modeset that disabled the CRTC.
    pub(crate) fn clear_legacy_mode_blob(&self) {
        let previous = self.legacy_mode.lock().take();
        if let Some((previous, _)) = previous {
            self.destroy_blob(previous);
        }
    }

    /// The legacy-mode blob the driver currently owns, if any.
    #[cfg(unittest)]
    pub(crate) fn legacy_mode_blob(&self) -> Option<BlobId> {
        self.legacy_mode.lock().as_ref().map(|(id, _)| *id)
    }

    /// Records `blob` as the legacy-mode blob and frees the previous one.
    ///
    /// Called after a successful `SETCRTC`: the driver created that blob, so it
    /// is the driver's to retire when the modeset it belongs to is replaced.
    /// Only the id is retired — the new state keeps the blob itself alive.
    pub(crate) fn replace_legacy_mode_blob(&self, id: BlobId, blob: Arc<Blob>) {
        let previous = self.legacy_mode.lock().replace((id, blob));
        if let Some((previous, _)) = previous
            && previous != id
        {
            self.destroy_blob(previous);
        }
    }

    /// Reserves room for one flip-complete event.
    ///
    /// The caller reserves before presenting and commits the reservation after
    /// a successful commit, so a full queue fails the ioctl instead of losing
    /// the event for a flip that already reached the screen.
    pub(crate) fn reserve_flip_event(&self) -> Option<EventReservation<'_>> {
        self.events.reserve_flip()
    }

    /// Current vblank sequence number.
    pub(crate) fn vblank_sequence(&self) -> u32 {
        self.events.sequence()
    }

    /// Advances the vblank sequence after a wait.
    pub(crate) fn advance_vblank(&self, count: u32) {
        self.events.advance(count);
    }

    /// Keeps the pages of a mapped dumb buffer alive, keyed by its mmap offset
    /// in bytes (the key the mapping interface reports).
    pub(crate) fn retain_mapping(&self, offset_bytes: u64, pages: Arc<PageRef>) {
        self.retained_pages.lock().insert(offset_bytes, pages);
    }

    fn read_events(&self, buf: &mut [u8]) -> DrmResult<usize> {
        self.backend()?;
        self.events.read(buf)
    }

    fn poll_events(&self) -> IoEvents {
        if self.backend().is_err() {
            return IoEvents::ERR | IoEvents::HUP;
        }
        self.events.poll()
    }
}

/// One character device of a card.
pub(crate) struct DrmMinor {
    device: Arc<DrmDevice>,
    kind: MinorKind,
}

impl DrmMinor {
    fn resolve(&self) -> DrmResult<Arc<DrmDevice>> {
        Ok(self.device.clone())
    }
}

impl DeviceFileOps for DrmMinor {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let state = self.resolve()?.open_file(self.kind)?;
        file.set_private_data(state);
        Ok(())
    }

    fn release(&self, _inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        state.dev.release_master_on_close(&state);
        // `drm_fb_release()`: framebuffers are per-file objects here, so a file
        // that exits without RMFB must not leave them in the device registry
        // holding their GEM objects forever. Removing them unbinds the plane
        // and keeps any scanned resource retained until the next present.
        let owned_fbs: Vec<FbId> = state.fbs.lock().iter().copied().collect();
        for fb in owned_fbs {
            let _ = state.dev.remove_framebuffer(&state, fb);
        }
        // `drm_gem_release()` + `drm_property_destroy_user_blobs()`: closing
        // the file drops every handle and destroys every blob it created.
        // A resource that a framebuffer or the scanout still names stays
        // alive through that reference and is retired with it.
        state.handles.lock().clear();
        // One commit section covers the legacy-slot clear, the device registry
        // and the file's own set; see `destroy_file_blobs`.
        state.dev.destroy_file_blobs(&state);
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        Ok(self.resolve()?.read_events(buf)?)
    }

    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        let device = self.resolve()?;
        device.backend()?;
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        Ok(crate::model::ioctl::dispatch(&device, &state, cmd, arg)?)
    }

    fn mmap(&self, file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        let device = self.resolve()?;
        device.backend()?;
        let state = file
            .private_data_get::<DrmFile>()
            .ok_or(VfsError::InvalidInput)?;
        let offset_bytes = mapper.offset() as u64;
        // The offset table is per open file: a render node cannot map a buffer
        // it never created, and a handle is the only way to name the buffer.
        let (_, object) = state
            .handle_by_offset_bytes(offset_bytes)
            .ok_or(VfsError::InvalidInput)?;
        let pages = object.pages();
        let phys = pages.phys_addr();
        let size_bytes = object.length_bytes();
        let range = PhysAddrRange::from_start_size(phys, size_bytes);
        // Retain only after the mapper accepts the range: the retention table
        // lives for the device's lifetime, so retaining before a failed mmap
        // (for example an over-long request) would pin the GEM backing forever.
        mapper.map_physical(range)?;
        device.retain_mapping(offset_bytes, pages);
        Ok(())
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn poll(&self, _file: &VfsFile) -> Option<IoEvents> {
        Some(self.device.poll_events())
    }

    fn register_poll(
        &self,
        _file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        self.device.events.register(context, events)
    }
}

/// `DRM_IOCTL_SET_MASTER`.
pub(crate) fn set_master(dev: &DrmDevice, file: &Arc<DrmFile>) -> DrmResult<usize> {
    dev.set_master(file)?;
    Ok(0)
}

/// `DRM_IOCTL_DROP_MASTER`.
pub(crate) fn drop_master(dev: &DrmDevice, file: &Arc<DrmFile>) -> DrmResult<usize> {
    dev.drop_master(file)?;
    Ok(0)
}
