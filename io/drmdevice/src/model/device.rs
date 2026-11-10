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
use core::sync::atomic::{AtomicU32, Ordering};

use minor::DrmMinor;

use super::{
    flip::{Flip, PendingFlip, take_reservation},
    vblank::{VblankClockState, VblankRequest, VblankState, resolve_absolute, resolve_relative},
    vma::VmaOffsetManager,
};
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
    /// Flips waiting for the blank that completes them.
    ///
    /// A card without a blank timeline never queues here: its flips complete
    /// with the commit, as Linux sends them on a device without vblank.
    pub(crate) flip_queue: Mutex<Vec<PendingFlip<'static>>>,
    /// The vertical blanking timeline and the display source that advances it.
    pub(crate) vblank: VblankState,
    /// The connector's `EDID` blob slot.
    edid_blob: Mutex<Option<BlobId>>,
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
    retained_pages: Mutex<BTreeMap<u64, Arc<GemObject>>>,
    next_dumb: AtomicU32,
    /// The next 3D context id (`virtgpu` handles).
    next_context: AtomicU32,
    /// Global GEM names (`dev->object_name_idr`).
    ///
    /// A name holds no reference: an object is in the table only while it is
    /// alive, and the name goes away with it, exactly as
    /// `drm_gem_flink_ioctl` documents. The table also allocates names.
    pub(crate) object_names: Mutex<ObjectNames>,
    /// The card's own `Weak`, so an object released during the card's own
    /// teardown can still unbind its offset without a live `Arc`.
    self_ref: Weak<Self>,
    /// The mmap offset namespace; `vma.rs` owns its rules.
    pub(crate) vma: VmaOffsetManager,
    /// Offset -> the object it names.
    ///
    /// Weak, like the handle tables: the offset is an object's identity while
    /// it lives, never a reason for it to live.
    vma_objects: Mutex<BTreeMap<u64, Weak<GemObject>>>,
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
        Arc::new_cyclic(|weak| Self {
            backend,
            framebuffers: Mutex::new(IdRegistry::new(FIRST_FB_ID)),
            blobs: Mutex::new(IdRegistry::new(FIRST_BLOB_ID)),
            state: Mutex::new(KmsState::default()),
            commit: Mutex::new(()),
            scanout: Mutex::new(ScanoutSlot::default()),
            master: Mutex::new(MasterState::default()),
            events: EventQueue::new(),
            flip_queue: Mutex::new(Vec::new()),
            self_ref: weak.clone(),
            vblank: VblankState::new(&weak.clone()),
            edid_blob: Mutex::new(None),
            in_formats: Mutex::new(None),
            legacy_mode: Mutex::new(None),
            hotplug: Mutex::new(None),
            retained_pages: Mutex::new(BTreeMap::new()),
            next_dumb: AtomicU32::new(FIRST_DUMB_HANDLE),
            next_context: AtomicU32::new(FIRST_CONTEXT_ID),
            object_names: Mutex::new(ObjectNames::default()),
            vma: VmaOffsetManager::default(),
            vma_objects: Mutex::new(BTreeMap::new()),
            next_resource: AtomicU32::new(0x1000),
        })
    }

    /// Handle for one minor of this card.
    ///
    /// `unit` is the minor number the node was created with. Linux derives the
    /// card's `EVENT` unit from it — the same device published on two nodes
    /// (`cardN` and `renderD*`) keeps one `EVENT` unit — and that unit is what
    /// `SYNCOBJ_EVENTFD` signal accounting is keyed by.
    pub fn minor(self: &Arc<Self>, kind: MinorKind, unit: u32) -> Arc<dyn DeviceFileOps> {
        // The minor owns the card: the cdev is the last thing that goes away
        // when devfs removes the node, so the object graph cannot outlive it or
        // be collected while the node is still openable.
        Arc::new(DrmMinor::new(self.clone(), kind, unit))
    }

    /// The reservation of one framebuffer, when it is still registered.
    ///
    /// A commit records itself as exclusive work there, so a client that fills
    /// a buffer the card displays waits for the display before it overwrites
    /// the pixels.
    pub(crate) fn framebuffer_resv(&self, fb: FbId) -> Option<Arc<crate::sync::dma_resv::DmaResv>> {
        self.framebuffers.lock().get(fb).map(|fb| fb.resv.clone())
    }

    /// Reserves `bound` as the exclusive work of one committed flip.
    ///
    /// The fence is the one userspace can wait for through the commit's
    /// out-fence (or the one that completes with the page-flip event), and it
    /// completes on the same point. Recording it on the framebuffer's
    /// reservation is what makes a writer wait for the display before it
    /// reuses the buffer.
    pub(crate) fn reserve_display(
        &self,
        bound: FbId,
        fence: Arc<crate::sync::dma_fence::DmaFence>,
    ) {
        if bound.is_none() {
            return;
        }
        if let Some(resv) = self.framebuffer_resv(bound) {
            let _previous = resv.set_write_fence(fence);
        }
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

    /// The output's EDID bytes, when the display reports them.
    pub(crate) fn edid(&self) -> Option<alloc::vec::Vec<u8>> {
        self.backend.edid()
    }

    /// The connector's `EDID` blob, created from the display's descriptor on
    /// first use.
    ///
    /// An immutable connector property has to keep its blob alive for the
    /// card's lifetime, so it is cached like `IN_FORMATS`; a device that
    /// reports no descriptor has no blob, and the property reads zero.
    pub(crate) fn connector_edid_blob(&self) -> Option<BlobId> {
        let mut cached = self.edid_blob.lock();
        if let Some(id) = cached.as_ref() {
            return Some(*id);
        }
        let bytes = self.edid()?;
        let (id, _) = self.create_blob(None, Arc::new(bytes), true).ok()?;
        *cached = Some(id);
        Some(id)
    }

    /// Whether the display's output is connected.
    pub(crate) fn is_connected(&self) -> bool {
        self.backend.is_connected()
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
    pub(crate) fn driver_info(&self) -> display::DriverInfo {
        self.backend.driver_info()
    }

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
        // A client parked on a blank of a display that no longer exists must
        // learn about it rather than sleep forever; no later blank can
        // re-register a waiter on a removed card.
        self.vblank.clock.remove();
    }

    /// A stable identity for this card, used as a dma-buf importer key.
    ///
    /// One card has one dma-buf importer, exactly as `drm_prime.c` registers
    /// one attachment per importer device; the address of the card is unique
    /// for as long as it exists, and an attachment cannot outlive its card.
    pub(crate) fn card_id(&self) -> u64 {
        self as *const Self as usize as u64
    }

    /// Allocates the next resource id.
    pub(crate) fn next_resource_id(&self) -> ResourceId {
        ResourceId::new(self.next_resource.fetch_add(1, Ordering::Relaxed))
    }

    /// The card's own weak handle.
    pub(crate) fn weak_self(&self) -> Weak<Self> {
        self.self_ref.clone()
    }

    /// The object registered at `offset`, if it is still alive.
    pub(crate) fn object_at_offset(&self, offset: u64) -> Option<Arc<GemObject>> {
        self.vma_objects.lock().get(&offset).and_then(Weak::upgrade)
    }

    /// Binds `object` to the offset it was given.
    ///
    /// This is `drm_vma_offset_add`: the offset names the object until the
    /// object is released.
    pub(crate) fn bind_offset(&self, offset: u64, object: &Arc<GemObject>) {
        self.vma_objects
            .lock()
            .insert(offset, Arc::downgrade(object));
    }

    /// Releases the offset binding of an object that is going away.
    pub(crate) fn unbind_offset(&self, offset: u64) {
        self.vma_objects.lock().remove(&offset);
        self.vma.release(offset);
    }

    /// Reserves the mmap offset one object is named by.
    ///
    /// This is `drm_vma_offset_add`: the object's mapping identity comes from
    /// the card, not from its size, so `MAP_DUMB` reports it and `mmap` resolves
    /// it back through the same table.
    pub(crate) fn allocate_mmap_offset(&self, size: u64) -> DrmResult<u64> {
        self.vma.allocate(size)
    }

    /// Allocates the next 3D context id.
    ///
    /// Context ids are the client's handle in `virtio-gpu` and must be unique
    /// per device; the counter only moves forward so a closed context's id is
    /// never handed to a later client.
    pub(crate) fn next_context_id(&self) -> u32 {
        self.next_context.fetch_add(1, Ordering::Relaxed)
    }

    /// Allocates the next dumb-buffer handle value.
    pub(crate) fn next_dumb_handle(&self) -> DumbHandle {
        DumbHandle::from_raw(self.next_dumb.fetch_add(1, Ordering::Relaxed))
    }

    /// Mutates the committed state and hands the flip to the card's
    /// completion point.
    ///
    /// This is the one commit path that userspace can wait for. It takes the
    /// whole [`Flip`], so the page-flip event slot and the out-fence reserved
    /// before the commit reach exactly one completion point:
    ///
    /// * the commit validates and publishes, then completes the flip at once on
    ///   a card without a blank timeline, or queues it for the blank that scans
    ///   it out;
    /// * a failed commit fails the out-fence with the error and releases the
    ///   event slot unused, so no descriptor this driver handed out hangs.
    pub(crate) fn commit_flip(
        &self,
        mutate: impl FnOnce(&mut KmsState) -> DrmResult<()>,
        mut flip: Flip<'_>,
    ) -> DrmResult<()> {
        match self.commit_with(mutate) {
            Ok(()) => {
                // SAFETY: the flip's event slot is tied to this card's event
                // queue, which outlives the commit that hands it over.
                unsafe { self.finish_flip(flip) };
                Ok(())
            }
            Err(error) => {
                // The commit never reached the screen: the out-fence reports the
                // same error the ioctl returns, and the event slot is released
                // unused.
                flip.fail(error);
                Err(error)
            }
        }
    }

    /// Queues or completes the page-flip event of one commit.
    ///
    /// With a blank timeline the event waits for the blank that completes the
    /// commit; without one the commit is the completion, and the event is
    /// published at once with sequence `0` and the card's current instant, as
    /// Linux's `drm_crtc_send_vblank_event` does on a device without vblank.
    ///
    /// # Safety
    ///
    /// The card must outlive the reservation the flip carries, which
    /// [`Flip`] ties to the card's event queue.
    pub(crate) unsafe fn finish_flip(&self, mut flip: Flip<'_>) {
        let event = flip.event.take();
        let fence_signal = flip.fence_signal.take();
        let user_data = flip.user_data;
        // SAFETY: the flip's event slot is tied to this card's event queue.
        let event = unsafe { take_reservation(event) };
        // An inactive CRTC has no blank to wait for: complete the flip at the
        // commit, as a card without a vblank timeline does, instead of leaving
        // its event and out-fence queued behind a stopped clock.
        if self.state.lock().crtc_active != 0 && self.vblank_supported() {
            self.flip_queue.lock().push(PendingFlip {
                event,
                fence: fence_signal,
                user_data,
            });
            return;
        }
        // No blank timeline: the commit is the completion, as
        // `drm_crtc_send_vblank_event` sends it on a device without vblank.
        if let Some(fence) = fence_signal {
            fence.signal();
        }
        let tick = self.vblank.clock.tick();
        if let Some(event) = event {
            event.commit_with(CRTC_ID, user_data, 0, tick.timestamp);
        }
    }

    /// The vertical blanking clock of this card.
    pub(crate) fn vblank_clock(&self) -> &Arc<VblankClockState> {
        &self.vblank.clock
    }

    /// Resolves a `WAIT_VBLANK` request against this file's cursor.
    ///
    /// A relative request counts from the sequence the file observed last; an
    /// absolute one is interpreted in the file's wrapping domain, so a stale
    /// target is answered as past instead of blocking for a full wrap.
    pub(crate) fn vblank_request(
        &self,
        file: &DrmFile,
        is_relative: bool,
        requested: u32,
    ) -> VblankRequest {
        let current = self.vblank.clock.tick().sequence;
        let mut tracker = file.vblank.lock();
        if is_relative {
            resolve_relative(&mut tracker, current, requested)
        } else {
            resolve_absolute(&mut tracker, current, requested)
        }
    }

    /// Whether this card can wait for blanks at all.
    ///
    /// Whether the card can answer vblank requests.
    ///
    /// A display that reports its own blanks drives the timeline directly;
    /// every other card drives it from the timer fallback, which is what Linux
    /// virtio-gpu does with `DRM_CRTC_VBLANK_TIMER_FUNCS`. Either way the
    /// timeline exists, so `WAIT_VBLANK` is servable and page-flip events are
    /// published on a blank instead of at commit time.
    pub(crate) fn vblank_supported(&self) -> bool {
        // Subscribe to the display's source when it has one; the timer covers
        // the cards that do not.
        let _ = self.start_scanout_source();
        true
    }

    /// Starts the timer-fallback timeline at `refresh_hz`.
    ///
    /// Called when a mode becomes `ACTIVE`; the thread itself is started once
    /// and parks while the CRTC is off, so a disable/enable cycle does not
    /// create a second ticker.
    pub(crate) fn start_vblank_timer(&self, refresh_hz: u32) {
        self.vblank
            .refresh_hz
            .store(refresh_hz.clamp(1, 240), Ordering::Relaxed);
        self.vblank.timer_on.store(true, Ordering::Release);
        if self.vblank.timer_started.swap(true, Ordering::AcqRel) {
            return;
        }
        let weak = self.weak_self();
        ktask::spawn_with_name(
            move || ktask::future::block_on(vblank_timer_task(weak)),
            alloc::string::String::from("drm-vblank"),
        );
    }

    /// Stops the timer-fallback timeline.
    pub(crate) fn stop_vblank_timer(&self) {
        self.vblank.timer_on.store(false, Ordering::Release);
    }

    /// Subscribes the card to the display's blanks and reports whether it did.
    ///
    /// The subscription happens once and stores the card's tick handler, which
    /// stays alive in [`VblankState`] for as long as the display may report.
    fn start_scanout_source(&self) -> DrmResult<()> {
        if self.vblank.scanout_started.load(Ordering::Acquire) {
            return Ok(());
        }
        let backend = self.backend()?;
        let Some(source) = backend.vblank_source() else {
            return Err(DrmError::OpNotSupported);
        };
        let callback: Arc<dyn Fn(display::VblankTick) + Send + Sync> = {
            let ticks = self.vblank.ticks();
            Arc::new(move |tick| ticks.edge(tick))
        };
        source.subscribe(callback).map_err(|_| DrmError::Io)?;
        self.vblank.scanout_started.store(true, Ordering::Release);
        Ok(())
    }

    /// Publishes the page-flip events that complete on a blank.
    ///
    /// The flips were queued with their reservation before the commit
    /// presented their resource; a blank completes every flip whose scanout
    /// reached the display before it.
    pub(crate) fn complete_flips(&self) {
        let flips = self.flip_queue.lock().drain(..).collect::<Vec<_>>();
        if flips.is_empty() {
            return;
        }
        let tick = self.vblank.clock.tick();
        for flip in flips {
            // The event and the fence share the blank: a client that waits for
            // the descriptor observes the flip reaching the display.
            if let Some(fence) = flip.fence {
                fence.signal();
            }
            if let Some(event) = flip.event {
                event.commit_with(CRTC_ID, flip.user_data, tick.sequence, tick.timestamp);
            }
        }
    }

    /// The retained object at `offset`, if the mapping still holds it.
    #[cfg(unittest)]
    pub(crate) fn retained_mapping(&self, offset_bytes: u64) -> Option<Arc<GemObject>> {
        self.retained_pages.lock().get(&offset_bytes).cloned()
    }

    /// Keeps the pages of a mapped dumb buffer alive, keyed by its mmap offset
    /// in bytes (the key the mapping interface reports).
    pub(crate) fn retain_mapping(&self, offset_bytes: u64, object: Arc<GemObject>) {
        // The object is what keeps the memory alive: an owned object keeps its
        // allocation, an imported one keeps the export that lent it. The
        // table stores it as itself, so the mapping and the object share one
        // reference count.
        self.retained_pages.lock().insert(offset_bytes, object);
    }
}

/// The device-wide global GEM names (`dev->object_name_lock` +
/// `object_name_idr`).
///
/// Entries are weak: the table never keeps an object alive, so a name stops
/// resolving as soon as the last handle or framebuffer referencing the object
/// is gone — which is what "the name does not hold a reference" means.
#[derive(Default)]
pub(crate) struct ObjectNames {
    names: BTreeMap<u32, Weak<GemObject>>,
    next: u32,
}

impl ObjectNames {
    /// The name an object was given, allocating one on first use.
    ///
    /// Names start at one, like `idr_alloc(..., 1, 0, ...)`, and are never
    /// reused while the object that owns them is alive.
    pub(crate) fn name_of(&mut self, object: &Arc<GemObject>) -> DrmResult<u32> {
        let known = object.name();
        if known != 0 && self.resolve(known).is_some() {
            return Ok(known);
        }
        let name = self.allocate()?;
        object.set_name(name)?;
        self.names.insert(name, Arc::downgrade(object));
        Ok(name)
    }

    /// The live object behind `name`.
    pub(crate) fn resolve(&mut self, name: u32) -> Option<Arc<GemObject>> {
        let weak = self.names.get(&name)?;
        let object = weak.upgrade();
        if object.is_none() {
            // The object is gone: the name goes with it.
            self.names.remove(&name);
        }
        object
    }

    /// The next unused name.
    fn allocate(&mut self) -> DrmResult<u32> {
        loop {
            self.next = self.next.checked_add(1).ok_or(DrmError::NoMemory)?;
            if !self.names.contains_key(&self.next) {
                return Ok(self.next);
            }
        }
    }
}

/// The timer-fallback blank source (`DRM_CRTC_VBLANK_TIMER_FUNCS`).
///
/// The thread ticks at the mode's refresh rate while the CRTC is active and
/// parks otherwise. It holds the card weakly, so the card's destruction ends
/// the thread at its next wake.
async fn vblank_timer_task(weak: Weak<DrmDevice>) {
    loop {
        let Some(device) = weak.upgrade() else {
            return;
        };
        let on = device.vblank.timer_on.load(Ordering::Acquire);
        let hz = device.vblank.refresh_hz.load(Ordering::Relaxed).max(1);
        drop(device);
        // A disabled CRTC ticks nothing; poll at a coarse rate to notice the
        // next enable without keeping the CPU awake.
        let wait_us = if on {
            (1_000_000u64 / u64::from(hz)).max(1_000)
        } else {
            50_000
        };
        ktask::future::sleep(TimeSpan::from_micros(wait_us)).await;
        if !on {
            continue;
        }
        let Some(device) = weak.upgrade() else {
            return;
        };
        if !device.vblank.timer_on.load(Ordering::Acquire) {
            continue;
        }
        let sequence = device.vblank.clock.tick_report().0.wrapping_add(1);
        let tick = display::VblankTick {
            sequence,
            timestamp: ktask::monotonic_time(),
        };
        device.vblank.ticks().edge(tick);
    }
}

impl Drop for DrmDevice {
    fn drop(&mut self) {
        // `flip_queue` holds event reservations borrowed from `events`, and
        // fields drop in declaration order with `events` first. Drain the
        // queue while the event queue is still alive so a pending flip never
        // touches freed memory.
        self.flip_queue.lock().clear();
    }
}
