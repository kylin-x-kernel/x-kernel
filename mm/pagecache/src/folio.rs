// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable folio ownership and synchronized content access.

use alloc::sync::Arc;
use core::{fmt, mem::MaybeUninit};

use kalloc::{UsageKind, global_allocator};
use kerrno::{KError, KResult};
use khal::{
    mem::{PhysAddr, VirtAddr, v2p},
    paging::{MappingFlags, PageSize, PageTableMut, PagingError},
};
use ksync::{Mutex, MutexGuard};
use log::warn;
use memaddr::PAGE_SIZE_4K;

use crate::{CacheResidency, PageIndex};

struct FolioCharge(Arc<dyn CacheResidency>);

impl fmt::Debug for FolioCharge {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("FolioCharge")
    }
}

impl Drop for FolioCharge {
    fn drop(&mut self) {
        self.0.remove_folio();
    }
}

/// A reference to one stable, initialized cache page object.
///
/// Removing the cache identity does not revoke this reference or freeze its
/// bytes. Copies remain serialized with every data access to that same object.
/// Mapping owners must validate file extent under their invalidation guard.
#[derive(Clone, Debug)]
pub struct PinnedFolio {
    folio: Arc<Folio>,
}

impl PinnedFolio {
    pub(crate) fn new(folio: Arc<Folio>) -> Self {
        Self { folio }
    }

    /// Consumes this pin and publishes a read-only PTE with an ownership token.
    ///
    /// Store the returned token in the mapping's backing object until PTE
    /// removal and TLB invalidation complete. Invalid flags or alignment publish no PTE.
    ///
    /// # Errors
    ///
    /// Rejects WRITE, DEVICE, UNCACHED and SHARED with PermissionDenied,
    /// unaligned addresses with InvalidInput, and existing PTEs with AlreadyExists.
    /// Other page-table errors retain their ordinary kernel error meaning.
    ///
    /// # Safety
    ///
    /// The caller must exclude file invalidation and validate the current file
    /// extent while publishing. The returned token must be released only after
    /// its PTE has been removed and the corresponding TLB invalidation completed.
    pub unsafe fn map_readonly(
        self,
        page_table: &mut PageTableMut,
        vaddr: VirtAddr,
        flags: MappingFlags,
    ) -> KResult<MappedFolio> {
        if flags.intersects(
            MappingFlags::WRITE
                | MappingFlags::DEVICE
                | MappingFlags::UNCACHED
                | MappingFlags::SHARED,
        ) {
            return Err(KError::PermissionDenied);
        }
        let paddr = self.folio.paddr();
        if !PageSize::Size4K.is_aligned(vaddr.as_usize())
            || !PageSize::Size4K.is_aligned(paddr.as_usize())
        {
            return Err(KError::InvalidInput);
        }
        page_table
            .map(vaddr, paddr, PageSize::Size4K, flags)
            .map_err(|error| match error {
                PagingError::NoMemory => KError::NoMemory,
                PagingError::NotAligned => KError::InvalidInput,
                PagingError::NotMapped | PagingError::InvalidAddress => KError::BadAddress,
                PagingError::AlreadyMapped => KError::AlreadyExists,
                PagingError::MappedToHugePage => KError::InvalidInput,
            })?;
        Ok(MappedFolio { pin: self })
    }

    /// Returns whether a page-table address refers to this stable page.
    pub fn matches_paddr(&self, paddr: PhysAddr) -> bool {
        self.folio.paddr() == paddr
    }

    /// Copies the original page, including after its cache identity is removed.
    ///
    /// The page's own mutex synchronizes the copy with all writers. Truncate
    /// may update a retained tail, so this is not an immutable snapshot. The
    /// caller must separately validate EOF when exposing bytes to a file mapping.
    /// Must run in sleepable task context without already holding this folio.
    ///
    /// The destination type requires exactly one page and expresses the full
    /// initialization guarantee without a runtime length check.
    pub fn copy_to_uninit(&self, destination: &mut [MaybeUninit<u8>; PAGE_SIZE_4K]) {
        let _guard = self.folio.lock();
        // SAFETY: this object owns the initialized allocation for the pin's
        // lifetime; its mutex serializes every safe access to these bytes.
        // The destination is writable for one page; ptr::copy permits overlap.
        unsafe {
            core::ptr::copy(
                self.folio.addr.as_ptr(),
                destination.as_mut_ptr().cast::<u8>(),
                PAGE_SIZE_4K,
            );
        }
    }
}

/// Ownership of a page published in a PTE.
///
/// Keep this token in the mapping's backing object until PTE removal and TLB
/// invalidation complete. Clones retain the same stable page object.
#[derive(Clone, Debug)]
#[must_use = "store the mapping token until PTE removal and TLB invalidation"]
pub struct MappedFolio {
    pin: PinnedFolio,
}

impl MappedFolio {
    /// Returns whether this mapping token owns the specified physical page.
    pub fn matches_paddr(&self, paddr: PhysAddr) -> bool {
        self.pin.matches_paddr(paddr)
    }

    /// Copies the retained page under its content lock; see PinnedFolio::copy_to_uninit.
    pub fn copy_to_uninit(&self, destination: &mut [MaybeUninit<u8>; PAGE_SIZE_4K]) {
        self.pin.copy_to_uninit(destination)
    }

    /// Publishes another mapping of this same retained object.
    ///
    /// # Safety
    ///
    /// The publication and returned token obey PinnedFolio::map_readonly's
    /// invalidation, file-extent and PTE/TLB lifetime requirements.
    pub unsafe fn map_readonly(
        &self,
        page_table: &mut PageTableMut,
        vaddr: VirtAddr,
        flags: MappingFlags,
    ) -> KResult<Self> {
        // SAFETY: the caller supplies the same publication and lifetime proof.
        unsafe { PinnedFolio::clone(&self.pin).map_readonly(page_table, vaddr, flags) }
    }
}

/// A stable page object combining allocation, immutable identity and its lock.
#[derive(Debug)]
pub struct Folio {
    index: PageIndex,
    addr: VirtAddr,
    state: Mutex<FolioState>,
}

#[derive(Debug, Default)]
struct FolioState {
    uptodate: bool,
    readahead: bool,
    dirty: bool,
    is_invalidated: bool,
    writeback: bool,
    charge: Option<FolioCharge>,
}

impl Folio {
    /// Allocates a zeroed, non-uptodate page not yet associated with a mapping.
    pub fn new_zeroed(index: PageIndex) -> KResult<Self> {
        let addr = global_allocator()
            .alloc_pages(1, PAGE_SIZE_4K, UsageKind::PageCache)
            .map_err(|_| KError::NoMemory)?;
        let addr = VirtAddr::from(addr);
        // SAFETY: the allocator returned one exclusively owned writable page.
        unsafe { core::ptr::write_bytes(addr.as_mut_ptr(), 0, PAGE_SIZE_4K) };
        Ok(Self {
            index,
            addr,
            state: Mutex::new(FolioState::default()),
        })
    }

    /// Returns the immutable page index, independently of cache residency.
    pub fn index(&self) -> PageIndex {
        self.index
    }

    /// Returns the physical allocation identity (existing raw consumers only).
    pub fn paddr(&self) -> PhysAddr {
        v2p(self.addr)
    }

    pub(crate) fn lock(&self) -> FolioGuard<'_> {
        FolioGuard {
            folio: self,
            state: self.state.lock(),
        }
    }

    pub(crate) fn try_lock(&self) -> Option<FolioGuard<'_>> {
        self.state
            .try_lock()
            .map(|state| FolioGuard { folio: self, state })
    }

    /// Accesses a page before publication; published objects expose only guards.
    pub fn data(&mut self) -> &mut [u8] {
        // SAFETY: exclusive ownership of the whole object excludes all guards
        // and pins; this initialized allocation is owned by the object.
        unsafe { core::slice::from_raw_parts_mut(self.addr.as_mut_ptr(), PAGE_SIZE_4K) }
    }

    /// Marks this privately owned page initialized before publication.
    pub fn mark_uptodate(&mut self) {
        self.lock().mark_uptodate();
    }

    /// Marks this privately owned page dirty before publication.
    pub fn mark_dirty(&mut self) {
        self.lock().mark_dirty();
    }

    /// Returns whether this page is dirty.
    pub fn is_dirty(&self) -> bool {
        self.lock().is_dirty()
    }

    /// Returns whether this page is initialized.
    pub fn is_uptodate(&self) -> bool {
        self.lock().is_uptodate()
    }
}

impl Drop for Folio {
    fn drop(&mut self) {
        let mut state = self.state.lock();
        if state.dirty || state.writeback {
            warn!(
                "dropping folio {} with dirty={} writeback={}",
                self.index, state.dirty, state.writeback
            );
        }
        let charge = state.charge.take();
        drop(state);
        global_allocator().dealloc_pages(self.addr.as_usize(), 1, UsageKind::PageCache);
        // Capacity becomes available only after the physical allocation is
        // gone. The provider callback holds neither the folio nor index lock.
        drop(charge);
    }
}

/// Restricted mutable access to one stable folio while its mutex is held.
///
/// It never exposes a mutable reference to the owning Folio, so callbacks
/// cannot replace or move out a published allocation or its synchronization.
pub struct FolioGuard<'a> {
    folio: &'a Folio,
    state: MutexGuard<'a, FolioState>,
}

impl FolioGuard<'_> {
    pub(super) fn charge_residency(&mut self, residency: Arc<dyn CacheResidency>) -> KResult<()> {
        debug_assert!(self.state.charge.is_none());
        residency.try_add_folio()?;
        self.state.charge = Some(FolioCharge(residency));
        Ok(())
    }

    /// Returns whether backing data has been initialized successfully.
    pub fn is_uptodate(&self) -> bool {
        self.state.uptodate
    }

    /// Makes a completely initialized folio available to cache readers.
    pub fn mark_uptodate(&mut self) {
        self.state.uptodate = true;
    }

    /// Marks this newly reserved page as a trigger for the next read window.
    pub fn mark_readahead(&mut self) {
        self.state.readahead = true;
    }

    /// Returns whether this folio carries a shared read-window trigger.
    pub fn is_readahead(&self) -> bool {
        self.state.readahead
    }

    /// Consumes a window trigger while holding the folio lock.
    /// Writeback pages retain the marker for a later read.
    pub fn take_readahead(&mut self) -> bool {
        if self.state.writeback {
            return false;
        }
        core::mem::take(&mut self.state.readahead)
    }

    /// Returns the immutable page index; this does not prove mapping residency.
    pub fn index(&self) -> PageIndex {
        self.folio.index
    }

    /// Marks a removed cache entry unusable by previously collected readers.
    pub(crate) fn invalidate(&mut self) {
        self.state.is_invalidated = true;
        self.clear_dirty();
    }

    pub(crate) fn is_invalidated(&self) -> bool {
        self.state.is_invalidated
    }

    /// Returns the physical address of this folio.
    pub fn paddr(&self) -> PhysAddr {
        self.folio.paddr()
    }

    /// Marks the folio dirty.
    pub fn mark_dirty(&mut self) {
        self.state.dirty = true;
    }

    /// Returns whether the folio is currently dirty.
    pub fn is_dirty(&self) -> bool {
        self.state.dirty
    }

    /// Returns whether a writeback pass is writing this folio.
    pub fn is_under_writeback(&self) -> bool {
        self.state.writeback
    }

    pub(super) fn start_writeback(&mut self) {
        debug_assert!(!self.state.writeback);
        self.state.writeback = true;
    }

    pub(super) fn end_writeback(&mut self) {
        debug_assert!(self.state.writeback);
        self.state.writeback = false;
    }

    /// Clears the dirty bit after successful writeback or truncation cleanup.
    pub fn clear_dirty(&mut self) {
        self.state.dirty = false;
    }

    /// Returns the folio contents as a mutable 4 KiB slice.
    pub fn data(&mut self) -> &mut [u8] {
        // SAFETY: the restricted guard holds this stable folio's mutex, so no
        // other Folio access can read or mutate this allocation during the
        // returned borrow. The guard borrows the stable object that owns the allocation.
        unsafe { core::slice::from_raw_parts_mut(self.folio.addr.as_mut_ptr(), PAGE_SIZE_4K) }
    }
}
