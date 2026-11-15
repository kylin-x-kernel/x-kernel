// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Stable folio ownership and synchronized content access.

use alloc::sync::Arc;
use core::{
    cell::UnsafeCell,
    mem::MaybeUninit,
    sync::atomic::{AtomicU8, Ordering},
};

use kalloc::{PageRef, UsageKind, alloc_zeroed_page_ref};
use kerrno::{KError, KResult};
use khal::{
    mem::{PhysAddr, VirtAddr, p2v},
    paging::{MappingFlags, PageSize, PageTableMut, PagingError},
};
use ktask::completion::CompletionWait;
use log::warn;
use memaddr::PAGE_SIZE_4K;

use crate::PageIndex;

mod wait;
use wait::{FolioWaitBit, WaitMode};
mod read_parts;
pub use read_parts::FolioReadPart;

const LOCKED: u8 = 1;
const WRITEBACK: u8 = 2;
const DIRTY: u8 = 8;
const INVALIDATED: u8 = 16;
const UPTODATE: u8 = 32;

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
    /// The PTE receives its own physical hold; its retirement belongs to the
    /// page table. The returned token retains this exact content object for COW.
    /// No access permissions returns None and installs no leaf or token.
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
    /// extent while publishing. Contents require their own access protocol;
    /// neither this token nor the PTE's lifetime hold grants write permission.
    pub unsafe fn map_readonly(
        self,
        page_table: &mut PageTableMut,
        vaddr: VirtAddr,
        flags: MappingFlags,
    ) -> KResult<Option<MappedFolio>> {
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
        let reference = self.folio.lock()?.try_clone_mapping_ref()?;
        let has_access =
            flags.intersects(MappingFlags::READ | MappingFlags::WRITE | MappingFlags::EXECUTE);
        page_table
            .map(
                vaddr,
                reference,
                if has_access {
                    flags
                } else {
                    MappingFlags::empty()
                },
            )
            .map_err(|error| match error {
                PagingError::NoMemory => KError::NoMemory,
                PagingError::NotAligned => KError::InvalidInput,
                PagingError::NotMapped | PagingError::InvalidAddress => KError::BadAddress,
                PagingError::AlreadyMapped => KError::AlreadyExists,
                PagingError::MappedToHugePage => KError::InvalidInput,
            })?;
        Ok(has_access.then_some(MappedFolio { pin: self }))
    }

    /// Returns whether a page-table address refers to this stable page.
    pub fn matches_paddr(&self, paddr: PhysAddr) -> bool {
        self.folio.paddr() == paddr
    }

    /// Copies the original page, including after its cache identity is removed.
    ///
    /// The page's own lock synchronizes the copy with all writers. Truncate
    /// may update a retained tail, so this is not an immutable snapshot. The
    /// caller must separately validate EOF when exposing bytes to a file mapping.
    /// Must run in sleepable task context without already holding this folio.
    ///
    /// The destination type requires exactly one page and expresses the full
    /// initialization guarantee without a runtime length check.
    ///
    /// # Errors
    /// Returns [`KError::InvalidInput`] if contention requires waiting outside
    /// sleepable task context. Failure leaves the destination untouched.
    pub fn copy_to_uninit(&self, destination: &mut [MaybeUninit<u8>; PAGE_SIZE_4K]) -> KResult<()> {
        let _guard = self.folio.lock()?;
        // SAFETY: this object owns the initialized allocation for the pin's
        // lifetime; its lock serializes every safe access to these bytes.
        // The destination is writable for one page; ptr::copy permits overlap.
        unsafe {
            core::ptr::copy(
                self.folio.addr().as_ptr(),
                destination.as_mut_ptr().cast::<u8>(),
                PAGE_SIZE_4K,
            );
        }
        Ok(())
    }
}

/// Stable content identity for a file page published in a managed PTE.
///
/// Clones retain the same content object and its lock for private-file COW.
/// The PTE independently owns its physical hold. Removing this token never
/// waits for TLB retirement and cannot release a page still held by a PTE.
#[derive(Clone, Debug)]
#[must_use = "retain the content token while private COW may read this page"]
pub struct MappedFolio {
    pin: PinnedFolio,
}

impl MappedFolio {
    /// Returns whether this mapping token owns the specified physical page.
    pub fn matches_paddr(&self, paddr: PhysAddr) -> bool {
        self.pin.matches_paddr(paddr)
    }

    /// Copies the retained page under its content lock; see PinnedFolio::copy_to_uninit.
    pub fn copy_to_uninit(&self, destination: &mut [MaybeUninit<u8>; PAGE_SIZE_4K]) -> KResult<()> {
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
    ) -> KResult<Option<Self>> {
        // SAFETY: the caller supplies the same publication and lifetime proof.
        unsafe { PinnedFolio::clone(&self.pin).map_readonly(page_table, vaddr, flags) }
    }
}

/// A stable page object combining allocation, immutable identity and its lock.
#[derive(Debug)]
pub struct Folio {
    index: PageIndex,
    page: PageRef,
    flags: AtomicU8,
    state: UnsafeCell<FolioState>,
}

// SAFETY: bytes and non-atomic state require the unique folio lock token.
// Acquire/Release transfers that access between tokens, independently of tasks.
// Lock-free reads use immutable identity or atomic flags; final drop is exclusive.
unsafe impl Sync for Folio {}

#[derive(Debug, Default)]
struct FolioState {
    readahead: bool,
}

impl Folio {
    /// Allocates a zeroed, non-uptodate page not yet associated with a mapping.
    pub fn new_zeroed(index: PageIndex) -> KResult<Self> {
        let page = alloc_zeroed_page_ref(UsageKind::PageCache).map_err(|_| KError::NoMemory)?;
        Ok(Self {
            index,
            page,
            flags: AtomicU8::new(0),
            state: UnsafeCell::new(FolioState::default()),
        })
    }

    fn addr(&self) -> VirtAddr {
        p2v(self.page.phys_addr())
    }

    /// Returns the immutable page index, independently of cache residency.
    pub fn index(&self) -> PageIndex {
        self.index
    }

    /// Returns the physical allocation identity (existing raw consumers only).
    pub fn paddr(&self) -> PhysAddr {
        self.page.phys_addr()
    }

    pub(crate) fn lock(&self) -> KResult<FolioGuard<'_>> {
        if let Some(guard) = self.try_lock() {
            return Ok(guard);
        }
        let wait = CompletionWait::prepare()?;
        Ok(self.lock_with(&wait))
    }

    pub(crate) fn lock_killable(&self) -> KResult<FolioGuard<'_>> {
        if let Some(guard) = self.try_lock() {
            return Ok(guard);
        }
        let wait = CompletionWait::prepare()?;
        self.lock_killable_with(&wait)
    }

    pub(super) fn lock_killable_with(&self, wait: &CompletionWait) -> KResult<FolioGuard<'_>> {
        if !self.wait_for_bit(FolioWaitBit::Locked, wait, WaitMode::Killable) {
            return Err(KError::Interrupted);
        }
        Ok(self.locked_guard())
    }

    // The operation validates sleepable context before any mutation requiring
    // mandatory cleanup. Its callbacks restore that context before returning.
    pub(super) fn lock_with(&self, wait: &CompletionWait) -> FolioGuard<'_> {
        self.wait_for_bit(FolioWaitBit::Locked, wait, WaitMode::Uninterruptible);
        self.locked_guard()
    }

    pub(crate) fn try_lock(&self) -> Option<FolioGuard<'_>> {
        self.acquire_lock().then(|| self.locked_guard())
    }

    fn acquire_lock(&self) -> bool {
        let acquired = self.flags.fetch_or(LOCKED, Ordering::Acquire) & LOCKED == 0;
        #[cfg(feature = "stats")]
        if acquired {
            wait::FOLIO_STATS.record_acquisitions(1);
        }
        acquired
    }

    fn locked_guard(&self) -> FolioGuard<'_> {
        // Both callers just acquired the unique lock token.
        FolioGuard { folio: self }
    }

    fn unlock(&self) {
        // Publish initialized bytes and state before another token acquires
        // access. No task-owned mutex or mapping lock participates in completion.
        self.clear_bit_and_wake(FolioWaitBit::Locked);
    }

    pub(super) fn is_under_writeback(&self) -> bool {
        self.flags.load(Ordering::Acquire) & WRITEBACK != 0
    }

    pub(super) fn wait_writeback(&self, wait: &CompletionWait) {
        // A shared bit wait observes one completion. Without the folio lock,
        // writeback may restart before this task runs, so recheck here.
        while self.is_under_writeback() {
            self.wait_for_bit(FolioWaitBit::Writeback, wait, WaitMode::Uninterruptible);
        }
    }

    // Cache residency can end while a read token retains initialization. Flags
    // keep detachment independent of that content lock; the token still owns
    // the allocation and all resources needed by completion, independently of
    // cache-residency quota released when its identity is detached.
    pub(super) fn invalidate(&self) {
        let _previous = self
            .flags
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |flags| {
                Some((flags | INVALIDATED) & !DIRTY)
            });
    }

    // Shared atomic state lets completion redirty without taking the content lock.
    pub(super) fn set_dirty(&self) {
        // Detached objects may still have pins, but cannot acquire new cache
        // dirtiness after the mapping owner has discarded their identity.
        let _previous = self
            .flags
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |flags| {
                (flags & INVALIDATED == 0).then_some(flags | DIRTY)
            });
    }

    // Publish completion with the mapping's writeback mark under its index
    // lock, then notify outside that lock when the returned hint is set.
    #[must_use = "wake writeback waiters after releasing the index lock"]
    pub(super) fn end_writeback(&self) -> bool {
        self.clear_bit(FolioWaitBit::Writeback)
    }

    pub(super) fn wake_writeback(&self) {
        self.wake_waiters(FolioWaitBit::Writeback);
    }

    /// Accesses a page before publication; published objects expose only guards.
    pub fn data(&mut self) -> &mut [u8] {
        // SAFETY: exclusive ownership of the whole object excludes all guards
        // and pins; this initialized allocation is owned by the object.
        unsafe { core::slice::from_raw_parts_mut(self.addr().as_mut_ptr(), PAGE_SIZE_4K) }
    }

    /// Marks this privately owned page initialized before publication.
    pub fn mark_uptodate(&mut self) {
        *self.flags.get_mut() |= UPTODATE;
    }

    /// Marks this privately owned page dirty before publication.
    pub fn mark_dirty(&mut self) {
        *self.flags.get_mut() |= DIRTY;
    }

    /// Returns whether this page is dirty.
    pub fn is_dirty(&self) -> bool {
        self.flags.load(Ordering::Acquire) & DIRTY != 0
    }

    /// Returns whether this page is initialized.
    pub fn is_uptodate(&self) -> bool {
        self.flags.load(Ordering::Acquire) & UPTODATE != 0
    }
}

impl Drop for Folio {
    fn drop(&mut self) {
        debug_assert_eq!(*self.flags.get_mut() & LOCKED, 0);
        let writeback = *self.flags.get_mut() & WRITEBACK != 0;
        let dirty = *self.flags.get_mut() & DIRTY != 0;
        if dirty || writeback {
            warn!(
                "dropping folio {} with dirty={} writeback={}",
                self.index, dirty, writeback
            );
        }
        // Cache quota follows the physical PageRef, not this cache identity:
        // page tables may keep the page alive after this Folio is dropped.
    }
}

/// Restricted mutable access while the folio's task-independent lock is held.
///
/// It never exposes a mutable reference to the owning Folio, so callbacks
/// cannot replace or move out a published allocation or its synchronization.
pub struct FolioGuard<'a> {
    folio: &'a Folio,
}

impl Drop for FolioGuard<'_> {
    fn drop(&mut self) {
        self.folio.unlock();
    }
}

impl FolioGuard<'_> {
    fn state(&self) -> &FolioState {
        // SAFETY: constructing this guard acquired the folio lock. The state
        // lives in the borrowed folio; no other owner can access it until drop.
        unsafe { &*self.folio.state.get() }
    }

    fn state_mut(&mut self) -> &mut FolioState {
        // SAFETY: the guard holds the unique folio lock token. This exclusive
        // borrow cannot coexist with another state borrow or outlive the guard.
        unsafe { &mut *self.folio.state.get() }
    }

    pub(super) fn into_read(self, folio: &Arc<Folio>) -> FolioRead {
        debug_assert!(core::ptr::eq(self.folio, Arc::as_ptr(folio)));
        debug_assert!(!self.is_uptodate());
        let read = FolioRead {
            folio: folio.clone(),
        };
        // Transfer the existing lock token; never unlock between publication
        // and handing initialization to the filesystem.
        core::mem::forget(self);
        read
    }

    /// Acquires a physical lifetime hold for one managed file mapping.
    ///
    /// Contents remain protected by this guard. The caller separately validates
    /// file extent and permissions under its invalidation guard before mapping.
    /// Returns NoMemory on reference overflow. Failure leaves the owner intact.
    pub fn try_clone_mapping_ref(&self) -> KResult<PageRef> {
        self.folio
            .page
            .try_clone_ref()
            .map_err(|_| KError::NoMemory)
    }

    /// Returns whether backing data has been initialized successfully.
    pub fn is_uptodate(&self) -> bool {
        self.folio.is_uptodate()
    }

    /// Makes a completely initialized folio available to cache readers.
    #[cfg(unittest)]
    pub(crate) fn mark_uptodate(&mut self) {
        self.folio.flags.fetch_or(UPTODATE, Ordering::Release);
    }

    /// Marks this newly reserved page as a trigger for the next read window.
    pub fn mark_readahead(&mut self) {
        self.state_mut().readahead = true;
    }

    /// Returns whether this folio carries a shared read-window trigger.
    pub fn is_readahead(&self) -> bool {
        self.state().readahead
    }

    /// Consumes a window trigger while holding the folio lock.
    /// Writeback pages retain the marker for a later read.
    pub fn take_readahead(&mut self) -> bool {
        if self.is_under_writeback() {
            return false;
        }
        core::mem::take(&mut self.state_mut().readahead)
    }

    /// Returns the immutable page index; this does not prove mapping residency.
    pub fn index(&self) -> PageIndex {
        self.folio.index
    }

    /// Marks a removed cache entry unusable by previously collected readers.
    pub(crate) fn invalidate(&mut self) {
        self.folio.invalidate();
    }

    pub(crate) fn is_invalidated(&self) -> bool {
        self.folio.flags.load(Ordering::Acquire) & INVALIDATED != 0
    }

    /// Returns the physical address of this folio.
    pub fn paddr(&self) -> PhysAddr {
        self.folio.paddr()
    }

    /// Marks the folio dirty unless its cache identity has been invalidated.
    pub fn mark_dirty(&mut self) {
        self.folio.set_dirty();
    }

    /// Returns whether the folio is currently dirty.
    pub fn is_dirty(&self) -> bool {
        self.folio.is_dirty()
    }

    /// Returns whether a writeback pass is writing this folio.
    pub fn is_under_writeback(&self) -> bool {
        self.folio.is_under_writeback()
    }

    pub(super) fn start_writeback(&mut self) {
        debug_assert!(!self.is_under_writeback());
        self.folio.flags.fetch_or(WRITEBACK, Ordering::Relaxed);
    }

    #[cfg(unittest)]
    pub(super) fn end_writeback(&mut self) {
        if self.folio.end_writeback() {
            self.folio.wake_writeback();
        }
    }

    /// Clears the dirty bit after successful writeback or truncation cleanup.
    pub fn clear_dirty(&mut self) {
        self.folio.flags.fetch_and(!DIRTY, Ordering::Release);
    }

    /// Returns the folio contents as a mutable 4 KiB slice.
    pub fn data(&mut self) -> &mut [u8] {
        // SAFETY: the restricted guard holds this stable folio's lock, so no
        // other Folio access can read or mutate this allocation during the
        // returned borrow. The guard borrows the stable object that owns the allocation.
        unsafe { core::slice::from_raw_parts_mut(self.folio.addr().as_mut_ptr(), PAGE_SIZE_4K) }
    }
}

/// Owns one locked folio until its read completes, possibly on another CPU.
///
/// This token retains the allocation and exclusive initialization access. It
/// contains no mapping pointer or second read state. Complete it only after all
/// I/O has relinquished the page; dropping it without completion leaves the
/// folio non-uptodate and wakes waiters so a later demand read can retry.
/// Implements [`block::BioMemory`] so submission can transfer this token and
/// completion can recover it from [`block::BioVec`] without copying the page.
#[must_use = "complete the folio after I/O or drop it to abandon initialization"]
pub struct FolioRead {
    folio: Arc<Folio>,
}

impl FolioRead {
    /// Returns this folio's immutable index.
    pub fn index(&self) -> PageIndex {
        self.folio.index()
    }

    /// Borrows the full page while the initialization token owns exclusive access.
    pub fn data(&mut self) -> &mut [u8] {
        // SAFETY: the token retains the stable allocation and the unique folio
        // lock. The exclusive borrow prevents completion while this slice lives.
        unsafe { core::slice::from_raw_parts_mut(self.folio.addr().as_mut_ptr(), PAGE_SIZE_4K) }
    }

    /// Marks this reserved page as a trigger for the next read window.
    pub fn mark_readahead(&mut self) {
        // SAFETY: initialization owns the unique folio lock token.
        unsafe { (*self.folio.state.get()).readahead = true };
    }

    /// Publishes whole-page read success, unlocks and wakes waiting users.
    ///
    /// Success requires every byte initialized, including holes and the EOF
    /// tail. Failure leaves the page non-uptodate. May run in task or ordinary
    /// I/O completion context (not NMI); it never acquires a sleepable lock.
    ///
    /// # Errors
    /// Returns the supplied read error unchanged after releasing the lock.
    pub fn complete(self, result: KResult<()>) -> KResult<()> {
        self.publish_result(result.is_ok());
        result
    }

    fn publish_result(&self, is_success: bool) {
        if is_success {
            self.folio.flags.fetch_or(UPTODATE, Ordering::Release);
        } else {
            self.folio.flags.fetch_and(!UPTODATE, Ordering::Release);
        }
    }
}

impl Drop for FolioRead {
    fn drop(&mut self) {
        self.folio.unlock();
    }
}

// SAFETY: the initialization token retains the allocation and exclusive folio
// lock until completion/drop. Moving the token does not move the page. Both
// views cover that same initialized allocation; their borrows exclude consuming
// the token, and Drop unlocks without sleeping in ordinary I/O completion context.
unsafe impl block::BioMemory for FolioRead {
    fn bytes(&self) -> &[u8] {
        // SAFETY: the token owns the stable page and its exclusive lock. This
        // shared borrow excludes data()/completion until the view expires.
        unsafe { core::slice::from_raw_parts(self.folio.addr().as_ptr(), PAGE_SIZE_4K) }
    }

    fn bytes_mut(&mut self) -> &mut [u8] {
        self.data()
    }
}
