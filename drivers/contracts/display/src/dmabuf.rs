// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The object behind a dma-buf descriptor.
//!
//! Linux's `struct dma_buf` is what an exporter publishes and every importer
//! reaches: the file descriptor owns it, an importer attaches to it, maps an
//! attachment to get memory, and reads the fences that order access. This
//! module is the kernel-wide contract for that object. It lives here — with the
//! display contracts, the only graphics vocabulary these crates share —
//! because both the exporter (a DRM driver) and any importer (a display
//! driver, a video device) have to name it, and neither may depend on the
//! other's types.
//!
//! The descriptor carries its object in the VFS file slot
//! [`kvfs::DMA_BUF_SLOT`], so an importer finds it without knowing which
//! filesystem created the descriptor. An export that cannot serve an importer
//! says so through its own [`DriverError`], never by pretending the descriptor
//! is not a dma-buf.

use alloc::sync::Arc;
use core::any::Any;

use kpoll::PollSet;
use kvfs::DMA_BUF_SLOT;

use crate::DriverError;

/// The memory one attachment maps: the exporter's pages and where they start.
///
/// An importer that can only address the guest's physical memory (a scanout
/// engine, a display controller) uses this directly. An export with no
/// guest-visible memory behind it returns [`DriverError::Unsupported`] from
/// its map operation instead of reporting a window it cannot guarantee.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct MappedMemory {
    /// Physical start address of the attachment's window.
    pub start: u64,
    /// Length of the window in bytes.
    pub len: u64,
}

/// One importer's attachment to an exported buffer (`struct dma_buf_attachment`).
///
/// The attachment names the export it belongs to. The export keeps whatever
/// bookkeeping it needs; the attachment itself is only the importer's handle
/// on it, so it can be passed back to the export to map or detach.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct DmaBufAttachment {
    /// Identity of the export this attachment was taken from.
    pub export: usize,
}

/// The object a dma-buf descriptor carries.
///
/// This is `struct dma_buf` as an importer sees it: identity, size, the
/// operations an importer calls into the exporter, and the fences that order
/// the memory. The exporter's private data is intentionally absent — an
/// importer must never need the exporter's types, which is why the export-side
/// downcast lives in [`DmaBufObject`].
pub trait ExportedDmaBuf: Send + Sync {
    /// A stable identity for the export, never zero.
    ///
    /// Linux compares `dma_buf` pointers to answer "the same buffer"; this is
    /// what an importer compares instead.
    fn identity(&self) -> usize;

    /// The exporter's name, `dma_buf->exp_name`.
    fn exporter(&self) -> &str;

    /// Buffer size in bytes, as `lseek(SEEK_END)` reports it.
    fn size(&self) -> u64;

    /// Attaches `importer` to the buffer.
    ///
    /// # Errors
    ///
    /// The exporter refuses an importer it cannot serve.
    fn attach(&self, importer: u64) -> Result<DmaBufAttachment, DriverError>;

    /// Detaches a previous attachment; a foreign attachment is ignored.
    fn detach(&self, attachment: &DmaBufAttachment);

    /// Maps `attachment` to memory the importer may use.
    ///
    /// # Errors
    ///
    /// The export has no importer-visible memory.
    fn map(&self, attachment: &DmaBufAttachment) -> Result<MappedMemory, DriverError>;

    /// The fences that must complete before an access of `write` intent.
    ///
    /// A reader waits for the exclusive work only, a writer for everything;
    /// this is `dma_resv_get_fences` for one usage, as opaque fence objects.
    fn fences(&self, write: bool) -> alloc::vec::Vec<Arc<dyn Any + Send + Sync>>;

    /// The readiness source a fence completion wakes (`dma_buf_poll`).
    fn poll(&self) -> &PollSet;

    /// The fence that orders any access, as a single point.
    ///
    /// An importer that keeps its own reservation needs one fence to record;
    /// this is `dma_resv_get_singleton` over the export's reservation, which
    /// an exporter answers with an already-signaled fence when it has none.
    fn fence_or_signaled(&self) -> Arc<dyn Any + Send + Sync>;
}

/// A concrete export, recoverable by its exporter.
///
/// The descriptor slot stores a trait object, so the exporter needs one
/// narrowing step to get its own buffer back — the equivalent of checking
/// `dma_buf->ops == &drm_gem_prime_dmabuf_ops` and casting `dma_buf->priv`.
pub trait DmaBufObject: ExportedDmaBuf {
    /// The export as `Any`, for the exporter's own downcast.
    ///
    /// This is the exporter's equivalent of `dma_buf->priv`: the DRM driver
    /// recovers its buffer structure from it.
    fn as_any(&self) -> &dyn Any;
}

/// The one type stored in the dma-buf slot.
///
/// `Arc::downcast` only recovers a concrete type, so the slot holds a small
/// named wrapper around the export's trait object instead of the bare object:
/// the wrapper type is what every reader downcasts to, and its field is the
/// export everyone shares.
pub struct DmaBufSlot(pub Arc<dyn DmaBufObject>);

impl DmaBufObject for DmaBufSlot {
    fn as_any(&self) -> &dyn Any {
        self.0.as_any()
    }
}

impl ExportedDmaBuf for DmaBufSlot {
    fn identity(&self) -> usize {
        self.0.identity()
    }

    fn exporter(&self) -> &str {
        self.0.exporter()
    }

    fn size(&self) -> u64 {
        self.0.size()
    }

    fn attach(&self, importer: u64) -> Result<DmaBufAttachment, DriverError> {
        self.0.attach(importer)
    }

    fn detach(&self, attachment: &DmaBufAttachment) {
        self.0.detach(attachment)
    }

    fn map(&self, attachment: &DmaBufAttachment) -> Result<MappedMemory, DriverError> {
        self.0.map(attachment)
    }

    fn fences(&self, write: bool) -> alloc::vec::Vec<Arc<dyn Any + Send + Sync>> {
        self.0.fences(write)
    }

    fn poll(&self) -> &PollSet {
        self.0.poll()
    }

    fn fence_or_signaled(&self) -> Arc<dyn Any + Send + Sync> {
        self.0.fence_or_signaled()
    }
}

/// Publishes `object` on the descriptor being built.
///
/// Called by the exporter right after it creates the dma-buf file, before the
/// descriptor becomes reachable by another process.
pub fn publish(file: &mut impl FileSlotWriter, object: Arc<dyn DmaBufObject>) {
    file.set_slot(DMA_BUF_SLOT, Arc::new(DmaBufSlot(object)));
}

/// The object behind a descriptor, if it is a dma-buf.
///
/// This is the importer's entry point, the counterpart of following
/// `file->f_op` in Linux.
pub fn find(file: &kvfs::VfsFile) -> Option<Arc<dyn DmaBufObject>> {
    let slot = file.slot_get::<DmaBufSlot>(DMA_BUF_SLOT)?;
    Some(slot.0.clone())
}

/// The one operation [`publish`] needs from a file under construction.
///
/// Implemented by the VFS file builder; kept as a local trait so this contract
/// crate does not have to name every builder detail.
pub trait FileSlotWriter {
    /// Stores `value` in the file's `slot`.
    fn set_slot(&mut self, slot: kvfs::FileSlot, value: Arc<dyn Any + Send + Sync>);
}

impl FileSlotWriter for kvfs::VfsFileBuilder {
    fn set_slot(&mut self, slot: kvfs::FileSlot, value: Arc<dyn Any + Send + Sync>) {
        kvfs::VfsFileBuilder::set_slot(self, slot, value);
    }
}
