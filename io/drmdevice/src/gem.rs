// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! GEM objects behind the dumb-buffer ABI and the dma-buf exchange
//! (`drm_gem.c`, `drm_prime.c`).
//!
//! The object owns its backing pages, its scanout resource, the knowledge of
//! how to free that resource and the reservation that orders its memory.
//! Everything else — handles, framebuffers, the scanout slot — holds an
//! `Arc<GemObject>`, so "a resource must outlive every handle that named it and
//! the scanout that displays it" needs no bookkeeping: it is the last
//! reference that destroys the resource.
//!
//! ## Sharing
//!
//! `PRIME_HANDLE_TO_FD` exports the backing as a dma-buf descriptor that
//! carries the object's reservation. Importing it on this card does not create
//! a second reservation: the object is recreated over the *same* pages and the
//! *same* reservation, because the memory is guest memory and both names
//! describe the same bus master's view of it. An importer therefore inherits
//! the exporter's implicit ordering instead of inventing a parallel one.

use core::sync::atomic::{AtomicU32, Ordering};

use display::{ScanoutBacking, ScanoutWindow};

use crate::{
    model::dma_buf::{Attachment, DmaBuf, DmaBufBacking},
    prelude::*,
    sync::dma_resv::DmaResv,
};

/// How a GEM object reaches its own memory.
///
/// The two forms follow the two ways an object comes into existence: a dumb
/// buffer allocates its pages, and an import of another export borrows a
/// window of that export's memory. Only the first form owns memory, so only it
/// can be held as a `PageRef`; the second borrows the export's allocation and
/// keeps the attachment that guarantees it outlives the object.
enum GemMemory {
    /// Pages this object owns.
    Owned {
        /// The allocation.
        pages: Arc<PageRef>,
        /// Byte offset of the object's memory inside the allocation.
        start: usize,
    },
    /// A window of another export's memory.
    Window {
        /// Physical start of the window.
        start: u64,
        /// Window length in bytes; the object's own `length_bytes` is derived
        /// from it at construction, so this is only the borrowed extent.
        #[expect(
            dead_code,
            reason = "CG-024-lint-exceptions: kept for diagnostics of the borrowed extent"
        )]
        len: u64,
    },
}

impl GemMemory {
    /// Physical address of the object's first byte.
    fn base(&self) -> u64 {
        match self {
            Self::Owned { pages, start } => pages.phys_addr().as_usize() as u64 + *start as u64,
            Self::Window { start, .. } => *start,
        }
    }

    /// The owned pages, when this object owns its memory.
    fn owned_pages(&self) -> Option<&Arc<PageRef>> {
        match self {
            Self::Owned { pages, .. } => Some(pages),
            Self::Window { .. } => None,
        }
    }
}

struct GemScanoutBacking<'a> {
    _memory: &'a GemMemory,
    range: PhysAddrRange,
}

// SAFETY: `GemScanoutBacking` is built from a live `GemMemory` owned by the
// calling GEM object; the borrow ties the range to that allocation for the
// duration of the display registration.
unsafe impl ScanoutBacking for GemScanoutBacking<'_> {
    fn range(&self) -> PhysAddrRange {
        self.range
    }
}

/// One allocation exposed through the dumb-buffer ABI.
pub(crate) struct GemObject {
    backend: BackendHandle,
    spec: ResourceSpec,
    /// Implicit ordering of the backing memory.
    ///
    /// Every consumer of the object sees the same reservation: the framebuffer
    /// that displays it and the importers that receive it through a dma-buf.
    resv: Arc<DmaResv>,
    /// The import this object was created from, keeping the dma-buf and its
    /// attachment alive for as long as the object references the memory.
    imported_from: Option<Attachment>,
    /// The attachment this object holds to another exporter's buffer.
    ///
    /// Dropping it (with the object) detaches this card from that export.
    foreign: Option<ForeignAttachment>,
    /// The export this object published, created on first use.
    ///
    /// Mirrors `drm_gem_object::dma_buf`: two exports of one object describe
    /// one buffer, so importing both resolves to one attachment.
    exported: Mutex<Option<Arc<DmaBuf>>>,
    /// Everything the object needs to reach its memory.
    ///
    /// An owned buffer's fresh allocation and an imported dma-buf's
    /// attachment-backed window are two forms of the same thing; keeping them
    /// apart is what lets an object be built over memory it does not own
    /// without pretending to own it.
    memory: GemMemory,
    /// Page-aligned mapping length, excluding allocation rounding slack.
    length_bytes: usize,
    /// Offset this object is mapped at, in bytes (page-aligned by construction).
    offset_bytes: u64,
    /// Handle the object was created under, for `CREATE_DUMB`.
    handle: DumbHandle,
    /// Resource id, taken exactly once by the destructor.
    resource: Mutex<Option<ResourceId>>,
    /// The card this object belongs to, for releasing its offset binding.
    device: Weak<DrmDevice>,
    /// Weak self-reference, published to exports for the PRIME fast path.
    self_ref: Weak<Self>,
    /// The global name this object was given (`drm_gem_object::name`).
    ///
    /// Assigned by the first `GEM_FLINK` and kept for the object's life; the
    /// device's name table maps it back to the object.
    name: AtomicU32,
    /// Blob memory kind (`VIRTGPU_BLOB_MEM_*`), zero for an ordinary object.
    blob_mem: AtomicU32,
    /// Whether the host owns the pixels of this object.
    ///
    /// A 3D resource is rendered into on the host through its context; its
    /// guest pages are only a backing store the host may never have written.
    /// Presenting it must not push those pages over the host copy, which is
    /// what a plain 2D scanout resource does.
    host_rendered: bool,
}

impl GemObject {
    /// Allocates zeroed contiguous backing and creates the scanout resource.
    ///
    /// Geometry validation belongs to the ioctl layer; this function only
    /// assumes `width`/`height` are non-zero and `width * 4 * height` fits.
    pub(crate) fn create(dev: &DrmDevice, width: u32, height: u32) -> DrmResult<Arc<Self>> {
        let pitch = width.checked_mul(4).ok_or(DrmError::InvalidInput)?;
        let size_bytes = (pitch as u64)
            .checked_mul(height as u64)
            .ok_or(DrmError::InvalidInput)?;
        if size_bytes as usize > DUMB_BUFFER_MAX_SIZE {
            return Err(DrmError::NoMemory);
        }
        let length_bytes = (size_bytes as usize).next_multiple_of(PAGE_SIZE_4K);
        let pages = alloc_zeroed_pages_exact_ref(
            length_bytes / PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            UsageKind::Global,
        )
        .map_err(|_| DrmError::NoMemory)?;
        let spec = ResourceSpec {
            width,
            height,
            pitch,
            format: ScanoutFormat::Bgra8888,
        };
        let offset_bytes = dev.allocate_mmap_offset(length_bytes as u64)?;
        Self::with_memory(
            dev,
            spec,
            length_bytes,
            offset_bytes,
            GemMemory::Owned {
                pages: Arc::new(pages),
                start: 0,
            },
            DmaResv::new(),
            None,
            true,
            false,
        )
        .inspect_err(|_| dev.unbind_offset(offset_bytes))
    }

    /// Allocates the guest backing of one blob resource.
    ///
    /// A blob has no scanout geometry: its size is the userspace request, and
    /// the object's spec only names the pitch the linear mapping exposes.
    pub(crate) fn create_blob_backing(dev: &DrmDevice, size_bytes: usize) -> DrmResult<Arc<Self>> {
        if size_bytes == 0 || size_bytes > DUMB_BUFFER_MAX_SIZE {
            return Err(DrmError::InvalidInput);
        }
        let spec = ResourceSpec {
            width: (size_bytes / 4).max(1) as u32,
            height: 1,
            pitch: size_bytes as u32,
            format: ScanoutFormat::Bgra8888,
        };
        Self::allocate_unbacked(dev, spec, size_bytes.next_multiple_of(PAGE_SIZE_4K), false)
    }

    fn allocate_unbacked(
        dev: &DrmDevice,
        spec: ResourceSpec,
        length_bytes: usize,
        host_rendered: bool,
    ) -> DrmResult<Arc<Self>> {
        let pages = alloc_zeroed_pages_exact_ref(
            length_bytes / PAGE_SIZE_4K,
            PAGE_SIZE_4K,
            UsageKind::Global,
        )
        .map_err(|_| DrmError::NoMemory)?;
        let offset_bytes = dev.allocate_mmap_offset(length_bytes as u64)?;
        Self::with_memory(
            dev,
            spec,
            length_bytes,
            offset_bytes,
            GemMemory::Owned {
                pages: Arc::new(pages),
                start: 0,
            },
            DmaResv::new(),
            None,
            false,
            host_rendered,
        )
        .inspect_err(|_| dev.unbind_offset(offset_bytes))
    }

    /// Binds the host resource a 3D caller created for this object.
    ///
    /// The destructor destroys the bound resource, so the caller must have
    /// attached this object's memory window to it before the object can be
    /// dropped; otherwise the host resource leaks behind the pages.
    pub(crate) fn set_resource(&self, resource: ResourceId) {
        self.resource.lock().replace(resource);
    }

    /// Builds an object over existing memory.
    ///
    /// The one construction path: a dumb buffer arrives with fresh pages and a
    /// fresh reservation, an imported dma-buf with the exporter's pages and the
    /// exporter's reservation.
    #[expect(
        clippy::too_many_arguments,
        reason = "CG-028-focused-functions: the constructor has one parameter per independent \
                  piece of the object wiring; a builder would hide which fields must agree"
    )]
    fn with_memory(
        dev: &DrmDevice,
        spec: ResourceSpec,
        length_bytes: usize,
        offset_bytes: u64,
        memory: GemMemory,
        resv: Arc<DmaResv>,
        imported_from: Option<Attachment>,
        create_backing: bool,
        host_rendered: bool,
    ) -> DrmResult<Arc<Self>> {
        let paddr = usize::try_from(memory.base()).map_err(|_| DrmError::InvalidInput)?;
        let resource = if create_backing {
            let resource = dev.next_resource_id();
            let backend = dev.backend()?.clone();
            let range = PhysAddrRange::try_from_start_size(paddr.into(), length_bytes)
                .ok_or(DrmError::InvalidInput)?;
            let backing = GemScanoutBacking {
                _memory: &memory,
                range,
            };
            backend.create_resource(resource, spec, &ScanoutWindow::new(&backing))?;
            Some(resource)
        } else {
            None
        };
        let backend = dev.backend()?.clone();
        let object = Arc::new_cyclic(|self_ref| Self {
            backend,
            spec,
            resv,
            imported_from,
            foreign: None,
            exported: Mutex::new(None),
            memory,
            length_bytes,
            offset_bytes,
            handle: dev.next_dumb_handle(),
            resource: Mutex::new(resource),
            device: dev.weak_self(),
            self_ref: self_ref.clone(),
            name: AtomicU32::new(0),
            blob_mem: AtomicU32::new(0),
            host_rendered,
        });
        dev.bind_offset(offset_bytes, &object);
        Ok(object)
    }

    /// Allocates the guest backing a `RESOURCE_CREATE` request names.
    ///
    /// Linux sizes the object from `params.size` (a zero size is one page) and
    /// the stride from `params.stride`, not from `width * 4`: a client may ask
    /// for row padding or a larger blob, and the backing must cover exactly
    /// what the host resource will reference.
    pub(crate) fn create_user_backing(
        dev: &DrmDevice,
        spec: ResourceSpec,
        size_bytes: usize,
    ) -> DrmResult<Arc<Self>> {
        let length_bytes = size_bytes.max(1).next_multiple_of(PAGE_SIZE_4K);
        Self::allocate_unbacked(dev, spec, length_bytes, true)
    }

    /// Whether the host rendered this object's pixels itself.
    pub(crate) const fn is_host_rendered(&self) -> bool {
        self.host_rendered
    }

    /// Blob memory kind of this object, zero for an ordinary GEM object.
    pub(crate) fn blob_mem(&self) -> u32 {
        self.blob_mem.load(Ordering::Relaxed)
    }

    /// Records the blob memory kind of this object.
    pub(crate) fn set_blob_mem(&self, blob_mem: u32) {
        self.blob_mem.store(blob_mem, Ordering::Relaxed);
    }

    /// Creates this card's object for an imported dma-buf.
    ///
    /// The import resolves to the same object the card already holds for that
    /// buffer, as `drm_gem_prime_import` does through the exporter's
    /// attachment lookup: the caller gets the existing object's mapping offset
    /// and the option to reuse its handle, and no second import is recorded.
    ///
    /// The import is recorded as an attachment on the dma-buf; the object owns
    /// it, so the attachment is detached exactly when the last reference to the
    /// imported memory goes away.
    pub(crate) fn import(
        dev: &DrmDevice,
        buf: &DmaBuf,
        spec: ResourceSpec,
        length_bytes: usize,
    ) -> DrmResult<(Arc<Self>, bool)> {
        // `drm_gem_prime_import` fast path: a buffer exported by this very
        // device resolves to the exporting object, so the resource id (and the
        // host resource the exporter rendered into) is shared instead of a
        // second object being created over the same pages.
        if let Some(exporter) = buf.exporter.as_ref().and_then(Weak::upgrade)
            && exporter
                .device
                .upgrade()
                .is_some_and(|owner| owner.card_id() == dev.card_id())
        {
            return Ok((exporter, true));
        }
        let attachment = buf.attach_to(dev.card_id(), |size| dev.allocate_mmap_offset(size))?;
        let offset_bytes = attachment.offset_bytes();
        let is_fresh = attachment.is_fresh();
        let object = Self::with_memory(
            dev,
            spec,
            length_bytes,
            offset_bytes,
            match &buf.backing.owner {
                Some(pages) => GemMemory::Owned {
                    pages: pages.clone(),
                    start: 0,
                },
                // A re-export of borrowed memory: the export keeps it alive, so
                // the object borrows the same window.
                None => GemMemory::Window {
                    start: buf.backing.start,
                    len: buf.backing.length as u64,
                },
            },
            buf.resv.clone(),
            Some(attachment),
            true,
            false,
        );
        let object = match object {
            Ok(object) => object,
            Err(error) => {
                if is_fresh {
                    dev.unbind_offset(offset_bytes);
                }
                return Err(error);
            }
        };
        Ok((object, is_fresh))
    }

    /// Creates this card's object over a window of another export's memory.
    ///
    /// This is what an importer builds after `attach` and `map`: the window is
    /// the object's memory, the export keeps it alive, and the object maps it
    /// through the linear map. The import owns no pages of its own — the
    /// export's allocation is what must survive — so the window form is used
    /// instead of a `PageRef`, which only exists for memory an allocator can
    /// account for.
    pub(crate) fn import_window(
        dev: &DrmDevice,
        window: display::MappedMemory,
        spec: ResourceSpec,
        resv: Arc<DmaResv>,
        attachment: ForeignAttachment,
    ) -> DrmResult<Arc<Self>> {
        if window.start == 0 || window.len == 0 {
            return Err(DrmError::InvalidInput);
        }
        let length_bytes = usize::try_from(window.len).map_err(|_| DrmError::NoMemory)?;
        if length_bytes > DUMB_BUFFER_MAX_SIZE || !window.start.is_multiple_of(PAGE_SIZE_4K as u64)
        {
            return Err(DrmError::InvalidInput);
        }
        let memory = GemMemory::Window {
            start: window.start,
            len: window.len,
        };
        let paddr = usize::try_from(memory.base()).map_err(|_| DrmError::InvalidInput)?;
        let resource = dev.next_resource_id();
        let backend = dev.backend()?.clone();
        let range = PhysAddrRange::try_from_start_size(paddr.into(), length_bytes)
            .ok_or(DrmError::InvalidInput)?;
        let backing = GemScanoutBacking {
            _memory: &memory,
            range,
        };
        backend.create_resource(resource, spec, &ScanoutWindow::new(&backing))?;
        let offset_bytes = dev
            .allocate_mmap_offset(length_bytes as u64)
            .inspect_err(|_| {
                let _ = dev.backend().map(|backend| {
                    let _ = backend.destroy_resource(resource);
                });
            })?;
        let object = Arc::new_cyclic(|self_ref| Self {
            backend,
            spec,
            resv,
            imported_from: None,
            foreign: Some(attachment),
            exported: Mutex::new(None),
            memory,
            length_bytes,
            offset_bytes,
            handle: dev.next_dumb_handle(),
            resource: Mutex::new(Some(resource)),
            device: dev.weak_self(),
            self_ref: self_ref.clone(),
            name: AtomicU32::new(0),
            blob_mem: AtomicU32::new(0),
            host_rendered: false,
        });
        dev.bind_offset(offset_bytes, &object);
        Ok(object)
    }

    /// The handle a second import of this object's dma-buf may reuse.
    ///
    /// The handle table is per file, so the caller decides by looking up the
    /// object's handle value; a foreign object's handle is never adopted.
    pub(crate) const fn import_handle(&self) -> DumbHandle {
        self.handle
    }

    /// The dma-buf view of this object, if its memory can be shared.
    ///
    /// Creates the export on first use and reuses it afterwards, as
    /// `drm_gem_prime_export` caches `obj->dma_buf`: two exports of one object
    /// describe one buffer, so importing both resolves to one attachment.
    ///
    /// Returns `None` for a host-private resource, which this driver never
    /// creates; the check keeps the boundary explicit for a future backend
    /// that could export a resource the guest cannot read.
    pub(crate) fn export_view(&self) -> Option<Arc<DmaBuf>> {
        // Re-exporting an imported buffer hands out the buffer it came from,
        // not a second view of the same pages.
        if let Some(import) = &self.imported_from {
            return Some(import.buf.clone());
        }
        let mut exported = self.exported.lock();
        if let Some(buf) = exported.as_ref() {
            return Some(buf.clone());
        }
        let driver_name = self
            .device
            .upgrade()
            .map(|device| device.driver_info().name)
            .unwrap_or("unknown");
        let buf = DmaBuf::new(
            DmaBufBacking {
                start: self.memory.base(),
                length: self.length_bytes,
                owner: self.memory.owned_pages().cloned(),
            },
            self.resv.clone(),
            crate::model::dma_buf::export_name(driver_name),
            self.spec.height,
            self.spec.width,
            Some(self.self_ref.clone()),
        );
        *exported = Some(buf.clone());
        Some(buf)
    }

    /// Pixel width.
    pub(crate) const fn width(&self) -> u32 {
        self.spec.width
    }

    /// Visible byte length of the object: the importer's `obj->size`.
    ///
    /// A framebuffer over this object must fit in it, which is the one
    /// geometry rule that survives importing: Linux's `drm_gem_fb_init_with_funcs`
    /// compares exactly this value against `offset + pitch * height`.
    pub(crate) const fn size_bytes(&self) -> u64 {
        self.length_bytes as u64
    }

    /// Pixel height.
    pub(crate) const fn height(&self) -> u32 {
        self.spec.height
    }

    /// Row stride in bytes.
    pub(crate) const fn pitch(&self) -> u32 {
        self.spec.pitch
    }

    /// The resource specification this object was created with.
    pub(crate) const fn spec(&self) -> ResourceSpec {
        self.spec
    }

    /// Offset this object is mapped at, in bytes, as `MAP_DUMB` reports it.
    pub(crate) const fn offset_bytes(&self) -> u64 {
        self.offset_bytes
    }

    /// Handle the object was created under.
    pub(crate) const fn handle(&self) -> DumbHandle {
        self.handle
    }

    /// The object's global name, or zero before `GEM_FLINK`.
    pub(crate) fn name(&self) -> u32 {
        self.name.load(Ordering::Acquire)
    }

    /// Records `name` as this object's global name.
    ///
    /// # Errors
    ///
    /// `EINVAL` when another name was already recorded: an object has one
    /// name for its whole life, so a racing assignment must not replace it.
    pub(crate) fn set_name(&self, name: u32) -> DrmResult<()> {
        self.name
            .compare_exchange(0, name, Ordering::AcqRel, Ordering::Acquire)
            .map(|_| ())
            .map_err(|_| DrmError::InvalidInput)
    }

    /// The object's reservation.
    pub(crate) fn resv(&self) -> &Arc<DmaResv> {
        &self.resv
    }

    /// The owned pages, when this object owns its memory.
    ///
    /// An object built over another export's window owns nothing; its memory
    /// is kept alive by the attachment and this returns `None`.
    #[cfg(unittest)]
    pub(crate) fn pages(&self) -> Option<Arc<PageRef>> {
        self.memory.owned_pages().cloned()
    }

    /// Physical start and length of the object's memory, for `mmap`.
    ///
    /// An owned object reports its allocation, an imported one the window it
    /// borrowed; both are what a mapper needs and neither hands out ownership.
    pub(crate) fn memory_range(&self) -> (u64, usize) {
        (self.memory.base(), self.length_bytes)
    }

    /// Page-aligned mapping length requested for this object's pixels.
    pub(crate) const fn length_bytes(&self) -> usize {
        self.length_bytes
    }

    /// The scanout resource id.
    pub(crate) fn resource(&self) -> DrmResult<ResourceId> {
        self.resource.lock().ok_or(DrmError::NotFound)
    }

    /// Creates the host resource a framebuffer over this object presents.
    ///
    /// The resource is a window into this object's pages: it starts at byte
    /// `offset` and covers `width` x `height` pixels at `pitch`. Only a
    /// framebuffer that covers the whole memory can present the object's own
    /// creation-time resource; every other layout needs a resource of its own,
    /// which is the split Linux's `drm_helper_mode_fill_fb_struct` makes
    /// between an object and a framebuffer's `obj[]`/`pitches[]`/`offsets[]`.
    ///
    /// # Errors
    ///
    /// `EINVAL` when the window does not fit the object, and the backend's
    /// error when the display cannot map that memory window.
    pub(crate) fn create_view_resource(
        &self,
        spec: ResourceSpec,
        offset: u64,
        id: ResourceId,
    ) -> DrmResult<()> {
        let start = self
            .memory
            .base()
            .checked_add(offset)
            .ok_or(DrmError::InvalidInput)?;
        let visible = u64::from(spec.pitch)
            .checked_mul(u64::from(spec.height))
            .ok_or(DrmError::InvalidInput)?;
        if offset
            .checked_add(visible)
            .is_none_or(|end| end > self.length_bytes as u64)
        {
            return Err(DrmError::InvalidInput);
        }
        let length = u32::try_from(visible).map_err(|_| DrmError::InvalidInput)?;
        let start = usize::try_from(start).map_err(|_| DrmError::InvalidInput)?;
        let range = PhysAddrRange::try_from_start_size(start.into(), length as usize)
            .ok_or(DrmError::InvalidInput)?;
        let backing = GemScanoutBacking {
            _memory: &self.memory,
            range,
        };
        self.backend
            .create_resource(id, spec, &ScanoutWindow::new(&backing))
    }

    /// Destroys a resource this object created for one of its framebuffers.
    ///
    /// The resource a framebuffer created is not created by the backend on
    /// the object's behalf alone: the scanout slot destroys it when the host
    /// stops scanning it, which is why this is not the object's destructor.
    /// A display that already went away has destroyed its own resources, so
    /// nothing is reported there.
    pub(crate) fn destroy_owned_resource(&self, resource: ResourceId) -> DrmResult<()> {
        if !self.backend.is_available() {
            return Ok(());
        }
        self.backend.destroy_resource(resource)
    }
}

impl Drop for GemObject {
    fn drop(&mut self) {
        // Freeing outside any caller's lock is the destructor's job; a backend
        // that already lost its device reports Io and there is nothing left to
        // do about it.
        if let Some(resource) = self.resource.lock().take()
            && let Err(error) = self.backend.destroy_resource(resource)
        {
            log::warn!("drm: cannot destroy resource {resource:?}: {error:?}");
        }
        // Dropping the attachment is the detach: the dma-buf and its exporter
        // outlive every importer. A foreign attachment detaches the same way,
        // through its own `Drop`.
        self.imported_from.take();
        self.foreign.take();
        // The offset stays reserved in the card's namespace but no longer
        // names anything, so a stale offset cannot map. The card may already
        // be gone when the last object is released; there is nothing to unbind
        // then.
        if let Some(device) = self.device.upgrade() {
            device.unbind_offset(self.offset_bytes);
        }
    }
}

/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`: same wire structure as the forward call.
#[repr(transparent)]
#[derive(Clone, Copy, bytemuck::AnyBitPattern, macros::UserRead, macros::UserWrite)]
pub(crate) struct DrmPrimeFdToHandle(pub(crate) DrmPrimeHandle);

/// `DRM_IOCTL_MODE_CREATE_DUMB`.
impl Ioctl for DrmModeCreateDumb {
    const NAME: &'static str = "MODE_CREATE_DUMB";

    fn handle(dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, c: &mut Self) -> DrmResult<usize> {
        // The backing store is always BGRA8888 (32bpp), but 24bpp requests must
        // still be accepted: libdrm-based clients on the non-atomic path ask for
        // 24bpp and hand that value back to ADDFB.
        if c.width == 0 || c.height == 0 || c.flags != 0 || !matches!(c.bpp, 24 | 32) {
            return Err(DrmError::InvalidInput);
        }
        if c.width > 16384 || c.height > 16384 {
            return Err(DrmError::InvalidInput);
        }
        let object = GemObject::create(dev, c.width, c.height)?;
        c.pitch = object.pitch();
        c.size = object.pitch() as u64 * object.height() as u64;
        c.handle = object.handle().raw();
        file.handles.lock().insert(object.handle(), object);
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_MAP_DUMB`.
impl Ioctl for DrmModeMapDumb {
    const NAME: &'static str = "MODE_MAP_DUMB";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, m: &mut Self) -> DrmResult<usize> {
        // Only an offset this file may map is reported, which mirrors the
        // per-file handle table `mmap` checks again.
        let object = file
            .handle(DumbHandle::from_raw(m.handle))
            .ok_or(DrmError::InvalidInput)?;
        m.offset = object.offset_bytes();
        Ok(0)
    }
}

/// `DRM_IOCTL_MODE_DESTROY_DUMB`.
impl Ioctl for DrmModeDestroyDumb {
    const NAME: &'static str = "MODE_DESTROY_DUMB";

    fn handle(_dev: &DrmDevice, file: &Arc<DrmFile>, _unit: u32, d: &mut Self) -> DrmResult<usize> {
        // `drm_mode_destroy_dumb_ioctl` is `drm_gem_handle_delete`: an unknown
        // or zero handle is `EINVAL`, and the handle is dropped whatever
        // object it names. The object itself is released when the last
        // reference (a framebuffer or the scanout) goes.
        file.take_handle(DumbHandle::from_raw(d.handle))
            .ok_or(DrmError::InvalidInput)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_GEM_CLOSE`.
impl Ioctl for DrmGemClose {
    const NAME: &'static str = "GEM_CLOSE";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        args: &mut Self,
    ) -> DrmResult<usize> {
        // `drm_gem_close_ioctl()` rejects a non-zero `pad` and reports an
        // unknown handle as EINVAL (through `drm_gem_handle_delete()`).
        if args.pad != 0 {
            return Err(DrmError::InvalidInput);
        }
        file.take_handle(DumbHandle::from_raw(args.handle))
            .ok_or(DrmError::InvalidInput)?;
        Ok(0)
    }
}

/// Whether a `PRIME_HANDLE_TO_FD` flag word is one Linux admits.
///
/// `drm_prime_handle_to_fd_ioctl` accepts exactly `DRM_CLOEXEC | DRM_RDWR` and
/// answers `EINVAL` for any other bit. `DRM_RDWR` is `O_RDWR`, and a client
/// exporting a buffer it also fills passes it together with `DRM_CLOEXEC`
/// (Mesa's `gbm_bo_get_fd_for_plane` passes `0x80002`).
///
/// The predicate is a separate function so the admitted set is testable
/// without a current task: the rest of the command installs a descriptor,
/// which needs one.
pub(crate) fn prime_export_flags_valid(flags: u32) -> bool {
    flags & !(DRM_CLOEXEC | DRM_RDWR) == 0
}

/// `DRM_IOCTL_PRIME_HANDLE_TO_FD`.
impl Ioctl for DrmPrimeHandle {
    const NAME: &'static str = "PRIME_HANDLE_TO_FD";

    fn handle(
        _dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        prime: &mut Self,
    ) -> DrmResult<usize> {
        // Rejecting `DRM_RDWR` failed every export Mesa makes: it exports the
        // buffers it renders into with both flags set.
        if !prime_export_flags_valid(prime.flags) {
            return Err(DrmError::InvalidInput);
        }
        // `drm_gem_prime_handle_to_fd` reports a handle that is not this
        // file's as `ENOENT` (the failed `drm_gem_object_lookup`).
        let object = file
            .handle(DumbHandle::from_raw(prime.handle))
            .ok_or(DrmError::NotFound)?;
        let Some(buf) = object.export_view() else {
            return Err(DrmError::OpNotSupported);
        };
        let cred = kprocess::current_cred();
        let file = crate::model::dma_buf_file::new_dma_buf_file(&buf, &cred)
            .map_err(|_| DrmError::NoMemory)?;
        let is_cloexec = prime.flags & DRM_CLOEXEC != 0;
        prime.fd = kprocess::current_files()
            .add_file(file, is_cloexec)
            .map_err(|_| DrmError::NoMemory)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_PRIME_FD_TO_HANDLE`.
impl Ioctl for DrmPrimeFdToHandle {
    const NAME: &'static str = "PRIME_FD_TO_HANDLE";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        prime: &mut Self,
    ) -> DrmResult<usize> {
        // `drm_gem_prime_fd_to_handle` starts from the descriptor: it must be
        // a live dma-buf (`ENOENT`, as Linux reports a failed `dma_buf_get`),
        // and the importer then asks the object for its memory.
        let exported = kprocess::current_files()
            .get_file(prime.0.fd)
            .map_err(|_| DrmError::NotFound)?;
        let export =
            crate::model::dma_buf_file::exported_object(&exported).ok_or(DrmError::NotFound)?;
        // This card's own buffer takes the fast path
        // (`drm_gem_prime_import_dev`'s own-dma_buf branch).
        if let Some(own) = export.as_any().downcast_ref::<DmaBuf>() {
            let identity = own.identity();
            return import_own_buffer(dev, file, prime, &export, identity);
        }
        import_foreign_buffer(dev, file, prime, &export)
    }
}

/// Imports this card's own export onto a new handle.
///
/// The buffer's memory is this card's own allocation, so the object is built
/// directly over it: same pages, same reservation. Matching by identity is what
/// tells "the buffer this card exported" from "a buffer that happens to look
/// alike"; the slot keeps the exporter's buffer alive for the whole call.
fn import_own_buffer(
    dev: &DrmDevice,
    file: &Arc<DrmFile>,
    prime: &mut DrmPrimeFdToHandle,
    export: &Arc<dyn display::DmaBufObject>,
    identity: usize,
) -> DrmResult<usize> {
    // The slot keeps the exporter's buffer alive for as long as the descriptor
    // exists, which is this whole call and every later user of the import.
    let own = export
        .as_any()
        .downcast_ref::<DmaBuf>()
        .ok_or(DrmError::InvalidInput)?;
    let spec = imported_spec(own)?;
    let existing = {
        // A file that already holds the card's object for this buffer names
        // it by that object's handle; a foreign object's handle is never
        // adopted.
        let handles = file.handles.lock();
        handles.iter().find_map(|(handle, known)| {
            known
                .export_view()
                .is_some_and(|known| known.identity() == identity)
                .then_some(*handle)
        })
    };
    if let Some(handle) = existing {
        prime.0.handle = handle.raw();
        return Ok(0);
    }
    let length = own.backing.length;
    let (object, _is_fresh) = GemObject::import(dev, own, spec, length)?;
    let handle = object.import_handle();
    file.handles.lock().insert(handle, object);
    prime.0.handle = handle.raw();
    Ok(0)
}

/// Imports a dma-buf another exporter produced.
///
/// This is `drm_gem_prime_import_dev` for a real dma-buf: attach, map the
/// attachment, and build this card's object over the resulting window. An
/// export with no memory this card can address refuses at `map`, which is
/// `EOPNOTSUPP` — "this importer cannot use that buffer", the class of answer
/// Linux gives when a driver has no import path at all.
fn import_foreign_buffer(
    dev: &DrmDevice,
    file: &Arc<DrmFile>,
    prime: &mut DrmPrimeFdToHandle,
    export: &Arc<dyn display::DmaBufObject>,
) -> DrmResult<usize> {
    let attachment = export
        .attach(dev.card_id())
        .map_err(|_| DrmError::OpNotSupported)?;
    let window = match export.map(&attachment) {
        Ok(window) => window,
        Err(_) => {
            export.detach(&attachment);
            return Err(DrmError::OpNotSupported);
        }
    };
    // A window is described as one long row: this card has no multi-plane
    // support, and a client lifts the geometry with ADDFB2, which now accepts
    // any framebuffer that fits its object.
    if window.len == 0 || window.len > DUMB_BUFFER_MAX_SIZE as u64 {
        export.detach(&attachment);
        return Err(DrmError::NoMemory);
    }
    let spec = ResourceSpec {
        width: u32::try_from(window.len / 4).map_err(|_| DrmError::InvalidInput)?,
        height: 1,
        pitch: u32::try_from(window.len / 4)
            .ok()
            .and_then(|width| width.checked_mul(4))
            .ok_or(DrmError::InvalidInput)?,
        format: ScanoutFormat::Bgra8888,
    };
    if spec.width == 0 {
        export.detach(&attachment);
        return Err(DrmError::InvalidInput);
    }
    // The object owns the attachment: when it goes, the export knows the
    // importer is gone. A failed import must not leave the attachment behind,
    // so it is dropped (and the export detached) on the error path.
    let attachment = ForeignAttachment::new(export, attachment);
    let resv = attachment.reservation()?;
    let object = GemObject::import_window(dev, window, spec, resv, attachment)?;
    let handle = object.import_handle();
    file.handles.lock().insert(handle, object);
    prime.0.handle = handle.raw();
    Ok(0)
}

/// One attachment this card holds to another export.
///
/// Dropping it detaches from the export, which is how
/// `drm_prime_gem_destroy` releases `obj->import_attach`: the object's own
/// lifetime decides when the exporter stops seeing this importer.
pub(crate) struct ForeignAttachment {
    export: Arc<dyn display::DmaBufObject>,
    attachment: display::DmaBufAttachment,
}

impl ForeignAttachment {
    /// Records one attachment of this card to `export`.
    pub(crate) fn new(
        export: &Arc<dyn display::DmaBufObject>,
        attachment: display::DmaBufAttachment,
    ) -> Self {
        Self {
            export: export.clone(),
            attachment,
        }
    }

    /// The ordering the export reports for its memory.
    ///
    /// The export's fences become this import's write ordering: the
    /// reservation is what every consumer of the object reads, and it must
    /// carry the producer's work so a later writer waits for it. A fence the
    /// importer cannot represent would silently drop that ordering, so the
    /// import is refused instead.
    fn reservation(&self) -> DrmResult<Arc<DmaResv>> {
        let fence = self
            .export
            .fence_or_signaled()
            .downcast::<crate::sync::dma_fence::DmaFence>()
            .map_err(|_| DrmError::OpNotSupported)?;
        let resv = DmaResv::new();
        let _ = resv.set_write_fence(fence);
        Ok(resv)
    }
}

impl Drop for ForeignAttachment {
    fn drop(&mut self) {
        self.export.detach(&self.attachment);
    }
}

/// The geometry an imported dma-buf is described with.
///
/// This driver's dma-buf is always a framebuffer-sized BGRA8888 window whose
/// rows are contiguous, so its width follows from its size and its height is
/// the exported framebuffer's; a buffer whose size is not a whole number of
/// rows cannot be a framebuffer and is refused instead of being described with
/// a rounded geometry.
fn imported_spec(buf: &DmaBuf) -> DrmResult<ResourceSpec> {
    let size = buf.backing.length;
    if size == 0 || size > DUMB_BUFFER_MAX_SIZE {
        return Err(DrmError::NoMemory);
    }
    // The exporter's geometry, not a guess from the byte length: the
    // framebuffer must describe the pixels that were shared, and ADDFB2 accepts
    // its own pitch for a buffer this card imported.
    let width = buf.width;
    let height = buf.height;
    let pitch = width.checked_mul(4).ok_or(DrmError::InvalidInput)?;
    if width == 0 || height == 0 || u64::from(pitch) * u64::from(height) > size as u64 {
        return Err(DrmError::InvalidInput);
    }
    Ok(ResourceSpec {
        width,
        height,
        pitch,
        format: ScanoutFormat::Bgra8888,
    })
}

/// `DRM_IOCTL_GEM_FLINK`.
///
/// This is `drm_gem_flink_ioctl`: the handle must name one of this file's
/// objects (`ENOENT` otherwise), and the object keeps one global name for its
/// whole life — the first `FLINK` allocates it and every later one returns
/// the same name. The name is the object's identity, not a reference: it
/// disappears with the last reference to the object, so a client cannot
/// resurrect a buffer through a stale name.
impl Ioctl for DrmGemFlink {
    const NAME: &'static str = "GEM_FLINK";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        args: &mut Self,
    ) -> DrmResult<usize> {
        let object = file
            .handle(DumbHandle::from_raw(args.handle))
            .ok_or(DrmError::NotFound)?;
        args.name = dev.flink_name(&object)?;
        Ok(0)
    }
}

/// `DRM_IOCTL_GEM_OPEN`.
///
/// This is `drm_gem_open_ioctl`: the name must still be alive (`ENOENT`), and
/// the reply is a fresh handle of the calling file plus the object's size.
/// Opening a name a file already owns adds a second handle to the same
/// object, exactly like `drm_gem_handle_create` does.
impl Ioctl for DrmGemOpen {
    const NAME: &'static str = "GEM_OPEN";

    fn handle(
        dev: &DrmDevice,
        file: &Arc<DrmFile>,
        _unit: u32,
        args: &mut Self,
    ) -> DrmResult<usize> {
        let object = dev.open_by_name(args.name as u64)?;
        let handle = dev.next_dumb_handle();
        file.handles.lock().insert(handle, object.clone());
        args.handle = handle.raw();
        args.size = object.size_bytes();
        Ok(0)
    }
}

impl DrmDevice {
    /// The global name of `object`, allocating one on first use.
    ///
    /// This is `drm_gem_flink_ioctl`'s name assignment: an object names itself
    /// once, and every later `GEM_FLINK` of it returns the same name.
    pub(crate) fn flink_name(&self, object: &Arc<GemObject>) -> DrmResult<u32> {
        self.object_names.lock().name_of(object)
    }

    /// The live object behind a global name.
    ///
    /// This is the lookup half of `drm_gem_open_ioctl`: a name whose object is
    /// gone is `ENOENT`, never resurrected.
    pub(crate) fn open_by_name(&self, name: u64) -> DrmResult<Arc<GemObject>> {
        let name = u32::try_from(name).map_err(|_| DrmError::NotFound)?;
        self.object_names
            .lock()
            .resolve(name)
            .ok_or(DrmError::NotFound)
    }
}

/// `DRM_IOCTL_GEM_CHANGE_HANDLE`: `drm_invalid_op`.
///
/// Linux reserves the command for CRIU and refuses it in the core; this driver
/// has no caller for it and refuses the same way.
impl Ioctl for DrmGemChangeHandle {
    const NAME: &'static str = "GEM_CHANGE_HANDLE";

    fn handle(
        _dev: &DrmDevice,
        _file: &Arc<DrmFile>,
        _unit: u32,
        _request: &mut Self,
    ) -> DrmResult<usize> {
        Err(DrmError::InvalidInput)
    }
}
