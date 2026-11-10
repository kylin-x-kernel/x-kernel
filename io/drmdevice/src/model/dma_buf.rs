// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! dma-buf: buffer sharing across drivers and devices (`drivers/dma-buf/`).
//!
//! The card's buffers are guest memory, so they are shared by *importing the
//! memory*, not by exporting a device-private object: the exporter receives
//! the physical backing under a dma-buf fd, and an importer that runs on the
//! same bus gets that same backing back. The export therefore carries exactly
//! what a consumer needs and nothing it may not have:
//!
//! * the read-only physical pages;
//! * the buffer's [`DmaResv`], so every attachment shares one implicit ordering
//!   for the memory rather than each importer inventing its own;
//! * no host visibility of any kind — a resource the host cannot read from the
//!   guest is not exported at all.
//!
//! An attachment records who holds the buffer and with which access. It stays
//! alive as long as the importer's GEM object does, and it is what the exporter
//! reports through `DMA_BUF_IOCTL_SYNC` bookkeeping.

use alloc::{collections::BTreeMap, string::String, sync::Arc};

use kspin::SpinNoIrq;

use super::error::{DrmError, DrmResult};
use crate::{
    abi::*,
    prelude::*,
    sync::{
        dma_fence::DmaFence,
        dma_resv::{DmaResv, ResvUsage},
        fence_access::{fence_from_fd, install},
        fence_list::FenceList,
        sync_file::c_string,
    },
};

/// The memory an export hands to an importer.
///
/// The backing is the exporter's pages. The `Arc` keeps the allocation alive
/// for the dma-buf's lifetime, so a GEM object may be destroyed while the
/// buffer is still exported.
#[derive(Clone)]
pub(crate) struct DmaBufBacking {
    /// Physical start of the exported window.
    pub(crate) start: u64,
    /// Length of the window in bytes.
    pub(crate) length: usize,
    /// The exporter's allocation, when the export owns its memory.
    ///
    /// It keeps the pages alive for every importer. An export of borrowed
    /// memory (a re-export of an import) has none: the export it came from
    /// stays alive through its own references.
    pub(crate) owner: Option<Arc<PageRef>>,
}

/// The device-independent part of an exported buffer.
pub(crate) struct DmaBuf {
    /// Exported memory.
    pub(crate) backing: DmaBufBacking,
    /// The owning `Arc`, set once by [`DmaBuf::new`].
    ///
    /// A buffer is only ever reachable as an `Arc<DmaBuf>`, and every holder
    /// that needs another handle receives it from here instead of rebuilding
    /// one — the reference counts stay the allocator's business.
    owner: SpinNoIrq<Weak<DmaBuf>>,
    /// Implicit ordering of the backing.
    pub(crate) resv: Arc<DmaResv>,
    /// Exporter's name for the buffer, reported in `DMA_BUF_IOCTL_INFO` and in
    /// `/proc` style diagnostics.
    pub(crate) name: String,
    /// Size in bytes, as `lseek` and `DMA_BUF_IOCTL_INFO` report it.
    pub(crate) size: u64,
    /// The object that exported this buffer, when it belongs to this driver.
    ///
    /// `drm_gem_prime_import` has a same-device fast path that returns this
    /// exact object, and the DRM object's resource id must stay the same or a
    /// renderer on another node (card0 vs renderD128) would receive a second
    /// resource that the exporter never rendered into.
    pub(crate) exporter: Option<alloc::sync::Weak<crate::gem::GemObject>>,
    /// Geometry of the exported framebuffer.
    ///
    /// A dma-buf has no geometry of its own in Linux; this driver exports only
    /// framebuffer-capable memory, and an importer needs the geometry to
    /// create its object without an ioctl that does not exist. The width is
    /// the pitch in pixels, so `width * 4 == pitch` and an importer can size a
    /// row without guessing from the byte length.
    pub(crate) height: u32,
    /// Visible width in pixels (`pitch / 4`).
    pub(crate) width: u32,
    /// The caller-set name (`dma_buf.name`).
    user_name: Mutex<String>,
    /// The readiness source every descriptor of this buffer shares: Linux
    /// keeps one wait queue per `dma_buf`, because the object is what fences
    /// complete on.
    poll: kpoll::PollSet,
    /// Importers that already attached to this buffer, by card identity.
    ///
    /// One importer has one attachment: a second import of the same buffer
    /// (through this export or a re-export of it) resolves to the existing
    /// import instead of creating a second mapping of one backing.
    attachments: Mutex<BTreeMap<u64, u64>>,
}

impl DmaBuf {
    /// Creates the exported view of one buffer.
    pub(crate) fn new(
        backing: DmaBufBacking,
        resv: Arc<DmaResv>,
        name: String,
        height: u32,
        width: u32,
        exporter: Option<alloc::sync::Weak<crate::gem::GemObject>>,
    ) -> Arc<Self> {
        let size = backing.length as u64;
        Arc::new_cyclic(|owner| Self {
            owner: SpinNoIrq::new(owner.clone()),
            backing,
            resv,
            name,
            size,
            height,
            width,
            exporter,
            user_name: Mutex::new(String::new()),
            poll: kpoll::PollSet::new(),
            attachments: Mutex::new(BTreeMap::new()),
        })
    }

    /// Another handle to this buffer.
    ///
    /// Only `None` between the cyclic construction and its publication, which
    /// no caller can observe: a buffer reaches callers only through the
    /// returned `Arc`.
    pub(crate) fn shared(&self) -> Option<Arc<Self>> {
        self.owner.lock().upgrade()
    }

    /// Attaches `importer` to the buffer, or resolves to its existing import.
    ///
    /// The returned attachment reports whether it created a new import and the
    /// mapping offset that import uses. An existing import keeps the offset of
    /// the first one, so one backing is never addressable under two names.
    pub(crate) fn attach_to(
        &self,
        importer: u64,
        allocate_offset: impl FnOnce(u64) -> DrmResult<u64>,
    ) -> DrmResult<Attachment> {
        let buf = self.shared().ok_or(DrmError::InvalidInput)?;
        let mut attachments = self.attachments.lock();
        if let Some(offset_bytes) = attachments.get(&importer) {
            return Ok(Attachment {
                buf,
                importer,
                offset_bytes: *offset_bytes,
                is_fresh: false,
            });
        }
        // The import's mapping offset must come from the importing card's VMA
        // namespace, exactly like an object the card allocated itself.
        let offset_bytes = allocate_offset(buf.backing.length as u64)?;
        attachments.insert(importer, offset_bytes);
        Ok(Attachment {
            buf,
            importer,
            offset_bytes,
            is_fresh: true,
        })
    }
}

/// One attachment of an exported buffer.
///
/// An importer holds the attachment for as long as its object references the
/// buffer; dropping it is the detach. This card is the only importer this
/// driver can create (the memory is guest memory, so every importer is the
/// same bus master), and the attachment records the importing card id.
pub(crate) struct Attachment {
    /// The exported buffer.
    pub(crate) buf: Arc<DmaBuf>,
    /// The card that attached.
    importer: u64,
    /// The mapping offset the import uses.
    offset_bytes: u64,
    /// Whether this attachment created the import.
    is_fresh: bool,
}

impl Attachment {
    /// The mapping offset of the import.
    pub(crate) const fn offset_bytes(&self) -> u64 {
        self.offset_bytes
    }

    /// Whether this attachment created the import.
    pub(crate) const fn is_fresh(&self) -> bool {
        self.is_fresh
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        // Detaching is what lets a later import of the same buffer create a
        // fresh one: the importer no longer holds the memory.
        let mut attachments = self.buf.attachments.lock();
        if attachments
            .get(&self.importer)
            .is_some_and(|offset| *offset == self.offset_bytes)
        {
            attachments.remove(&self.importer);
        }
    }
}

impl core::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Attachment")
            .field("name", &self.buf.name)
            .finish()
    }
}

impl DmaBuf {
    /// Exported name.
    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    /// This buffer's identity, as every importer sees it.
    ///
    /// The slot and every attachment hold an `Arc` to the buffer, so the
    /// address is stable while any descriptor or import of it can be resolved.
    pub(crate) fn identity(&self) -> usize {
        self as *const Self as usize
    }

    /// Renames the buffer (`dma_buf_set_name`).
    ///
    /// The name is what `sync_file_info`, `/proc` diagnostics and the anon
    /// inode publish; it has no effect on the memory.
    pub(crate) fn set_name(&self, name: String) {
        *self.user_name.lock() = name;
    }
}

/// The fences a consumer with `write` in mind must wait for
/// (`dma_resv_usage_rw`).
///
/// A reader waits for the exclusive work, a writer for everything; this is the
/// same set `dma_resv_get_singleton` and `dma_resv_wait_timeout` return, and
/// the same one `poll()` registers on.
pub(crate) fn usage_fences(buf: &DmaBuf, write: bool) -> Arc<FenceList> {
    buf.resv.snapshot(if write {
        ResvUsage::Write
    } else {
        ResvUsage::Read
    })
}

/// `DMA_BUF_IOCTL_SYNC`: begin or end CPU access.
///
/// The flags are validated exactly as `dma_buf_ioctl` does (any bit outside
/// `DMA_BUF_SYNC_VALID_FLAGS_MASK` is `EINVAL`, a direction of zero is
/// `EINVAL`, and `DMA_BUF_SYNC_RW` is a valid bidirectional access). `START`
/// is `dma_buf_begin_cpu_access`: it waits interruptibly for the reservation
/// in the direction the access names. `END` is `dma_buf_end_cpu_access`,
/// which this driver leaves as a no-op: its producers record their work on
/// the reservation themselves, so there is nothing to publish on end.
pub(crate) fn sync_fd(buf: &Arc<DmaBuf>, request: &DmaBufSync) -> DrmResult<()> {
    let flags = request.flags;
    if flags & !DMA_BUF_SYNC_VALID_FLAGS_MASK != 0 {
        return Err(DrmError::InvalidInput);
    }
    let direction = flags & DMA_BUF_SYNC_RW;
    if direction == 0 {
        return Err(DrmError::InvalidInput);
    }
    if flags & DMA_BUF_SYNC_END != 0 {
        return Ok(());
    }
    let fences = usage_fences(buf, direction & DMA_BUF_SYNC_WRITE != 0);
    crate::sync::wait::wait_for_fences(&fences)
}

/// `DMA_BUF_SET_NAME`: rename the buffer for tracking.
///
/// The name is copied with `strndup_user`'s `DMA_BUF_NAME_LEN` cap; an
/// over-long name is `EINVAL`, as a failure to copy it is there.
pub(crate) fn set_name_fd(buf: &Arc<DmaBuf>, name: posix_types::UserConstPtr<u8>) -> DrmResult<()> {
    let name = name
        .cast::<core::ffi::c_char>()
        .load_bytes_with_max_len(DMA_BUF_NAME_LEN)
        .map_err(|_| DrmError::InvalidInput)?;
    buf.set_name(c_string(&name));
    Ok(())
}

/// `DMA_BUF_IOCTL_EXPORT_SYNC_FILE`: export the buffer's current fences.
///
/// The exported fence is `dma_resv_get_singleton` of the reservation with the
/// usage the flags name (a write access waits for everything, a read access
/// for the writers), or a stub when nothing is outstanding. The flags describe
/// the *waiting* direction, so they are not `DMA_BUF_SYNC_END`/`NOWAIT`.
pub(crate) fn export_sync_file(
    buf: &Arc<DmaBuf>,
    request: &mut DmaBufExportSyncFile,
) -> DrmResult<()> {
    validate_sync_file_rw(request.flags as u64)?;
    let fences = usage_fences(buf, request.flags as u64 & DMA_BUF_SYNC_WRITE != 0);
    let fence = if fences.fences().is_empty() {
        private_stub()
    } else {
        merge_fences(&fences)
    };
    let cred = kprocess::current_cred();
    request.fd = install(&fence, &cred)?;
    Ok(())
}

/// `DMA_BUF_IOCTL_IMPORT_SYNC_FILE`: record a sync_file's fences.
///
/// This is `dma_buf_import_sync_file`: the descriptor names a sync_file, its
/// fence is unwrapped into its members, and every member is recorded with the
/// usage the flags name (a write access is exclusive work, a read access shared
/// work).
pub(crate) fn import_sync_file(buf: &Arc<DmaBuf>, request: &DmaBufImportSyncFile) -> DrmResult<()> {
    validate_sync_file_rw(request.flags as u64)?;
    // Resolve the fence before touching the reservation, so a bad fd leaves
    // the buffer's ordering unchanged.
    let fence = fence_from_fd(request.fd)?;
    let write = request.flags as u64 & DMA_BUF_SYNC_WRITE != 0;
    for member in crate::sync::fence_access::flatten(&fence) {
        if write {
            let _previous = buf.resv.add_write_fence(member);
        } else {
            let _previous = buf.resv.add_read_fence(member);
        }
    }
    Ok(())
}

/// Validates the read/write flags of the sync-file bridge.
///
/// Linux accepts only the direction bits here: any other bit is `EINVAL` and a
/// direction of zero is `EINVAL`, while `DMA_BUF_SYNC_RW` means "as a writer".
fn validate_sync_file_rw(flags: u64) -> DrmResult<()> {
    if flags & !DMA_BUF_SYNC_RW != 0 || flags & DMA_BUF_SYNC_RW == 0 {
        return Err(DrmError::InvalidInput);
    }
    Ok(())
}

/// Merges `fences` into the single fence a sync_file can carry
/// (`dma_fence_unwrap_merge` under `dma_resv_get_singleton`).
fn merge_fences(fences: &FenceList) -> Arc<DmaFence> {
    crate::sync::fence_access::unwrap_merge(fences.fences())
}

/// The stub an export hands out when the buffer has no outstanding fence.
fn private_stub() -> Arc<DmaFence> {
    crate::sync::fence_access::private_stub()
}

/// The name a buffer is exported under, `drm_<driver>` as Linux prints it.
pub(crate) fn export_name(driver: &str) -> String {
    alloc::format!("drm_{driver}")
}

impl display::ExportedDmaBuf for DmaBuf {
    fn identity(&self) -> usize {
        Self::identity(self)
    }

    fn exporter(&self) -> &str {
        &self.name
    }

    fn size(&self) -> u64 {
        self.size
    }

    fn attach(&self, _importer: u64) -> Result<display::DmaBufAttachment, display::DriverError> {
        // The generic importer takes its own attachment; this card's import
        // goes through `attach_to`, which reuses the one a repeated import of
        // the same buffer already holds.
        Ok(display::DmaBufAttachment {
            export: Self::identity(self),
        })
    }

    fn detach(&self, _attachment: &display::DmaBufAttachment) {
        // A generic attachment carries no importer identity, so there is
        // nothing per-importer to release; the card's own attachment detaches
        // through its `Attachment` guard.
    }

    fn map(
        &self,
        attachment: &display::DmaBufAttachment,
    ) -> Result<display::MappedMemory, display::DriverError> {
        if attachment.export != Self::identity(self) {
            return Err(display::DriverError::InvalidInput);
        }
        // This driver exports guest memory only, so the window an importer
        // maps is the exporter's own pages: an importer that can address the
        // guest scans exactly the memory the producer filled.
        Ok(display::MappedMemory {
            start: self.backing.start,
            len: self.backing.length as u64,
        })
    }

    fn fences(&self, write: bool) -> Vec<Arc<dyn core::any::Any + Send + Sync>> {
        usage_fences(self, write)
            .fences()
            .iter()
            .map(|fence| fence.clone() as Arc<dyn core::any::Any + Send + Sync>)
            .collect()
    }

    fn poll(&self) -> &kpoll::PollSet {
        &self.poll
    }

    fn fence_or_signaled(&self) -> Arc<dyn core::any::Any + Send + Sync> {
        // `dma_resv_get_singleton` over this export's write list: the work a
        // consumer must finish before the buffer is stable, or a fence that is
        // already done when nothing is outstanding.
        let fences = self.resv.write_list();
        match fences.fences().last() {
            Some(fence) => fence.clone() as Arc<dyn core::any::Any + Send + Sync>,
            None => crate::sync::dma_fence::new_signaled_fence()
                as Arc<dyn core::any::Any + Send + Sync>,
        }
    }
}

impl display::DmaBufObject for DmaBuf {
    fn as_any(&self) -> &dyn core::any::Any {
        self
    }
}
