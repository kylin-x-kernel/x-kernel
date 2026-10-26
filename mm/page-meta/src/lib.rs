// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Physical-page metadata and allocator-independent boot memory preparation.
//!
//! [`init`] reserves its own metadata storage from a caller-owned physical
//! range slice, leaving the remaining RAM for an allocator. PFN indexing and
//! metadata storage geometry are private implementation choices. Neither page
//! allocation nor page-content access is granted by descriptor lookup.
#![no_std]

mod backend;
mod free_ranges;

use core::ptr::NonNull;

use klazy::Once;
use memaddr::PhysAddrRange;

/// Failure to prepare or access physical-page metadata.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PageMetaError {
    /// An address, size or physical-page range is invalid or overflows.
    InvalidRange,
    /// No complete physical pages were supplied.
    EmptyCoverage,
    /// The private metadata backend capacity is exceeded.
    CapacityExceeded,
    /// Two nonempty physical ranges overlap.
    Overlap,
    /// Available RAM cannot satisfy the metadata backend's storage needs.
    NoMemory,
    /// A metadata storage buffer is too small for its initialization request.
    InsufficientStorage,
    /// A requested page or range is outside physical coverage.
    NotCovered,
    /// A global metadata table has already been published.
    AlreadyPublished,
}

/// Metadata for a physical base-page identity.
///
/// Identity survives allocation and release of its page contents. The first
/// stage records permanent metadata reservations, not allocation state or page
/// references. Representation and storage geometry remain private.
pub struct PageMeta {
    is_permanent: bool,
}

impl PageMeta {
    pub(crate) const fn new() -> Self {
        Self {
            is_permanent: false,
        }
    }

    pub(crate) fn mark_permanent(&mut self) {
        self.is_permanent = true;
    }

    /// Reports a permanent reservation installed before table publication.
    /// False does not establish that the page is free, including allocator-owned
    /// bitmap, heap or PCP pages whose ownership remains with that allocator.
    pub fn is_permanent(&self) -> bool {
        self.is_permanent
    }
}

/// Physical-page metadata table with a private lookup and storage backend.
///
/// Public operations do not expose sections, segments, array indexes, mapping
/// levels or backend capacity. The caller retains all backing until this value
/// and its lookup borrows are destroyed. Drop does not reclaim any RAM.
pub struct PageMetaTable {
    inner: backend::Table,
}

impl PageMetaTable {
    /// Returns the number of covered physical base pages, including metadata backing.
    pub fn covered_pages(&self) -> usize {
        self.inner.covered_pages()
    }

    /// Borrows metadata for a covered PFN, or returns None for a physical hole.
    /// The borrow grants no page ownership, content access or release right.
    pub fn lookup(&self, pfn: usize) -> Option<&PageMeta> {
        self.inner.lookup(pfn)
    }
}

/// Initializes page metadata, removing its storage from caller-owned free RAM.
///
/// `free` is normalized to complete 4 KiB pages, sorted and merged in place.
/// Successful storage reservations shrink these ranges; empty entries may remain
/// and must be skipped when handing the remaining RAM to an allocator. Physical
/// coverage includes the reserved metadata pages as well as the remaining RAM.
///
/// `map` is called only for reserved storage, with its complete physical range.
/// It must work before a heap exists; the described page contents need no mapping.
/// PFN indexing and storage geometry remain private backend choices.
///
/// # Returns
/// The initialized table. It does not reclaim its backing on drop.
///
/// # Errors
/// Rejects invalid, overlapping or empty input, backend capacity exhaustion,
/// insufficient RAM, or a mapping callback failure. The slice may be normalized
/// on error; earlier successful reservations remain removed if a later one fails.
/// A failed storage request is not deducted. No global table is published.
///
/// # Safety
/// The caller must exclusively own the RAM named by every nonempty input range
/// and exclude concurrent access during initialization. For each successful map
/// result, the pointer must provide exclusive writable access to the entire
/// requested range within one allocation and be aligned for PageMeta. Keep all
/// metadata backing and mappings valid until the table and all its borrows are
/// gone. Only the remaining ranges may subsequently be handed to an allocator.
pub unsafe fn init(
    free: &mut [PhysAddrRange],
    mut map: impl FnMut(PhysAddrRange) -> Result<NonNull<u8>, PageMetaError>,
) -> Result<PageMetaTable, PageMetaError> {
    free_ranges::normalize(free)?;
    // SAFETY: the caller owns every input RAM range exclusively and supplies
    // full-range writable mappings aligned for PageMeta whose storage outlives
    // the returned table.
    let inner = unsafe { backend::init(free, &mut map) }?;
    Ok(PageMetaTable { inner })
}

static GLOBAL: Once<PageMetaTable> = Once::new();

/// Publishes the boot-owned physical metadata table independently of allocators.
///
/// # Errors
/// Returns AlreadyPublished if a table is already installed.
///
/// # Safety
/// Serialize publication against other publication attempts. On success the
/// table's reserved backing must remain mapped, exclusive to this service,
/// and valid forever, including after any allocator instance is destroyed.
pub unsafe fn publish(table: PageMetaTable) -> Result<&'static PageMetaTable, PageMetaError> {
    if GLOBAL.get().is_some() {
        return Err(PageMetaError::AlreadyPublished);
    }
    Ok(GLOBAL.call_once(|| table))
}

/// Borrows metadata from the published physical table, or returns None
/// before publication and for PFNs outside its coverage.
pub fn lookup(pfn: usize) -> Option<&'static PageMeta> {
    GLOBAL.get()?.lookup(pfn)
}
