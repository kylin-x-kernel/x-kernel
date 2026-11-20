// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::fmt;

use kalloc::PageRef;
use kerrno::{KError, KResult};
use khal::{mem::PhysAddr, paging::PageSize};
use kspin::SpinNoIrq;
use ksync::Mutex;
use memaddr::PAGE_SIZE_4K;
use vmobj::{
    AnonObjectId, MappingView, MappingViewId, MappingViewNotifier, MappingViewRange,
    MappingViewSpec, ObjectInvalidateWork, VmObjectId, next_mapping_view_id,
};

use crate::ids::{AnonLineageId, next_anon_lineage_id, next_anon_object_id};

struct RegisteredView {
    view: MappingView,
    notifier: Option<Arc<dyn MappingViewNotifier>>,
}

struct AnonPrivatePageState {
    page: PageRef,
    logical_shares: SpinNoIrq<u32>,
}

/// Observation/source handle for one immutable private-page identity.
///
/// Cloning this handle retains physical backing without reserving a logical
/// COW share. Content access still needs the owning runtime's synchronization.
#[derive(Clone)]
pub struct AnonPrivatePageHandle {
    inner: Arc<AnonPrivatePageState>,
}

impl AnonPrivatePageHandle {
    /// Returns the backing frame address without granting content access.
    pub fn phys_addr(&self) -> PhysAddr {
        self.inner.page.phys_addr()
    }

    /// Returns the leaf size validated from this hold's selected page count.
    ///
    /// Compound backing may be larger; it does not change this selected extent.
    pub fn page_size(&self) -> PageSize {
        // Slot construction accepts only a complete supported leaf, and the
        // stored PageRef never changes its selected bounds afterward.
        match self.inner.page.page_count() {
            1 => PageSize::Size4K,
            512 => PageSize::Size2M,
            262_144 => PageSize::Size1G,
            _ => unreachable!("private page extent was validated at construction"),
        }
    }

    /// Returns a snapshot of whether exactly one logical slot owns this page.
    ///
    /// This is not authority to publish writable access later. The owning-MM
    /// mutex must exclude source fork/share reservation from this check through
    /// in-place reuse and writable publication. Split/moved views stay in that
    /// MM; fork creates a distinct child object whose shares are counted here.
    pub fn is_exclusive(&self) -> bool {
        *self.inner.logical_shares.lock() == 1
    }

    /// Acquires independent physical lifetime protection for a managed PTE.
    ///
    /// This does not reserve a logical COW share or grant content access. The
    /// caller retains the returned hold until mapping retirement completes.
    ///
    /// # Errors
    /// Returns `NoMemory` on physical-reference overflow.
    pub fn try_clone_mapping_ref(&self) -> KResult<PageRef> {
        self.inner
            .page
            .try_clone_ref()
            .map_err(|_| KError::NoMemory)
    }

    fn is_same_slot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

struct AnonPrivatePageSlot {
    handle: AnonPrivatePageHandle,
}

impl AnonPrivatePageSlot {
    fn new(page: PageRef, size: PageSize) -> KResult<Self> {
        if page.page_count().checked_mul(PAGE_SIZE_4K) != Some(size as usize)
            || !page.phys_addr().as_usize().is_multiple_of(size as usize)
        {
            return Err(KError::InvalidInput);
        }
        Ok(Self {
            handle: AnonPrivatePageHandle {
                inner: Arc::new(AnonPrivatePageState {
                    page,
                    logical_shares: SpinNoIrq::new(1),
                }),
            },
        })
    }

    fn try_clone_slot(&self) -> KResult<Self> {
        let mut shares = self.handle.inner.logical_shares.lock();
        *shares = shares.checked_add(1).ok_or(KError::NoMemory)?;
        Ok(Self {
            handle: self.handle.clone(),
        })
    }
}

impl Drop for AnonPrivatePageSlot {
    fn drop(&mut self) {
        let mut shares = self.handle.inner.logical_shares.lock();
        assert!(*shares > 0, "dropping unreferenced private page slot");
        *shares -= 1;
        // Release the count lock before the handle can drop physical backing.
    }
}

/// Removed logical COW share retained until old-owner accesses have ended.
///
/// Ordinary Drop ends this share and can expose a sibling as exclusive. The
/// runtime must retain it through confirmed old-PTE retirement, or prove old
/// accesses excluded. Unconfirmed completion must conservatively retain this
/// guard, even if separate PTE holds still prevent physical reclamation.
#[must_use = "retain the old logical share until access retirement completes"]
pub struct RetiredAnonPrivatePage {
    slot: AnonPrivatePageSlot,
}

impl RetiredAnonPrivatePage {
    /// Returns an observer of the retired identity without adding a COW share.
    pub fn handle(&self) -> &AnonPrivatePageHandle {
        &self.slot.handle
    }
}

/// One owned logical share reserved for a fork child object.
pub struct AnonPrivateForkPage {
    object_start: u64,
    slot: AnonPrivatePageSlot,
}

impl AnonPrivateForkPage {
    /// Returns the byte offset inside the anonymous object.
    pub const fn object_start(&self) -> u64 {
        self.object_start
    }

    /// Returns an observer of this reserved page.
    pub fn handle(&self) -> &AnonPrivatePageHandle {
        &self.slot.handle
    }
}

/// Prepared fork shares committed only after page-table installation completes.
///
/// Dropping uncommitted preparation releases its shares. The caller must first
/// exclude access to any unpublished child PTEs and restore parent permissions.
#[must_use]
pub struct PreparedAnonPrivateFork {
    child: Arc<AnonPrivateObject>,
    pages: Vec<AnonPrivateForkPage>,
}

impl PreparedAnonPrivateFork {
    /// Returns owned reservations whose mappings should be installed in the child.
    pub fn pages(&self) -> &[AnonPrivateForkPage] {
        &self.pages
    }

    /// Moves reserved logical slots into the unpublished child object.
    ///
    /// Parent write protection, child PTE installation and mapping completion
    /// must precede commit. Error cleanup must exclude child access before this
    /// preparation's shares end. No caller may mutate the child during commit.
    ///
    /// # Errors
    /// Returns `AlreadyExists` without publishing any slots on a child collision.
    pub fn commit(mut self) -> KResult {
        let mut child_pages = self.child.pages.lock();
        for page in &self.pages {
            if child_pages.contains_key(&page.object_start) {
                return Err(KError::AlreadyExists);
            }
        }
        for page in self.pages.drain(..) {
            let previous = child_pages.insert(page.object_start, page.slot);
            assert!(
                previous.is_none(),
                "prepared child page collided during commit"
            );
        }
        Ok(())
    }
}

/// Prepared absent slot, published only after a successful runtime PTE update.
#[must_use]
pub struct PreparedAnonPrivatePage {
    object: Arc<AnonPrivateObject>,
    object_start: u64,
}

impl PreparedAnonPrivatePage {
    /// Consumes typed backing and publishes one absent logical slot.
    ///
    /// The caller must have excluded access to any separate mapping while this
    /// fallible publication is pending. Runtime mapping uses `commit_after`.
    /// Rejected backing drops automatically; no raw release is required.
    ///
    /// # Errors
    /// Returns `InvalidInput` for mismatched physical geometry, or `AlreadyExists`
    /// if another slot was installed after preparation.
    pub fn commit(self, page: PageRef, size: PageSize) -> KResult<AnonPrivatePageHandle> {
        let slot = AnonPrivatePageSlot::new(page, size)?;
        let mut pages = self.object.pages.lock();
        if pages.contains_key(&self.object_start) {
            return Err(KError::AlreadyExists);
        }
        let handle = slot.handle.clone();
        pages.insert(self.object_start, slot);
        Ok(handle)
    }

    /// Consumes typed backing and publishes it after an atomic PTE update.
    ///
    /// The object slot remains locked and absent while `commit` receives an
    /// observer of the candidate. Clone mapping holds before PTE mutation. The
    /// callback must leave the PTE unchanged on error and must not reenter the
    /// object map. Success is the publication boundary: no fallible work may
    /// remain between PTE publication and this slot insertion. A rejected
    /// candidate drops after releasing the map lock.
    ///
    /// # Errors
    /// Returns `InvalidPage` for mismatched geometry, `Changed` for a populated
    /// slot, or `Commit` for a callback failure. None publishes an object slot.
    pub fn commit_after<E>(
        self,
        page: PageRef,
        size: PageSize,
        commit: impl FnOnce(&AnonPrivatePageHandle) -> Result<(), E>,
    ) -> Result<AnonPrivatePageHandle, AnonPrivatePageCommitError<E>> {
        let slot = AnonPrivatePageSlot::new(page, size)
            .map_err(|_| AnonPrivatePageCommitError::InvalidPage)?;
        let mut pages = self.object.pages.lock();
        if pages.contains_key(&self.object_start) {
            return Err(AnonPrivatePageCommitError::Changed);
        }
        commit(&slot.handle).map_err(AnonPrivatePageCommitError::Commit)?;
        let handle = slot.handle.clone();
        pages.insert(self.object_start, slot);
        Ok(handle)
    }
}

/// Error returned while committing a private page publication or replacement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum AnonPrivatePageCommitError<E> {
    /// The object slot no longer has the expected absent or matching-page state.
    Changed,
    /// The backing range size or physical alignment does not match `PageSize`.
    InvalidPage,
    /// The caller-provided runtime commit step failed without PTE publication.
    Commit(E),
}

struct AnonPrivateViewRegistration {
    object: Arc<AnonPrivateObject>,
    id: MappingViewId,
}

/// Lifetime guard for one registered anonymous-private view.
#[derive(Clone)]
pub struct AnonPrivateViewGuard {
    inner: Arc<AnonPrivateViewRegistration>,
}

impl AnonPrivateViewGuard {
    /// Returns the stable registration id kept alive by this guard.
    pub fn id(&self) -> MappingViewId {
        self.inner.id
    }
}

impl Drop for AnonPrivateViewRegistration {
    fn drop(&mut self) {
        self.object.unregister_view(self.id);
    }
}

/// Private anonymous identity, views, and logical page-slot owner.
///
/// Runtime slot operations require task context and the owning-MM mutex. It
/// serializes source fork/share reservation with exclusive check-and-reuse;
/// split/moved views must remain within that MM. Fork children use distinct
/// objects. Object-map callbacks must not reenter this object. Destruction must
/// follow owning-MM unmap/access exclusion, before its logical shares can end.
pub struct AnonPrivateObject {
    id: AnonObjectId,
    lineage: AnonLineageId,
    views: Mutex<BTreeMap<MappingViewId, RegisteredView>>,
    pages: Mutex<BTreeMap<u64, AnonPrivatePageSlot>>,
}

impl fmt::Debug for AnonPrivateObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let view_count = self.views.lock().len();
        f.debug_struct("AnonPrivateObject")
            .field("id", &self.id)
            .field("lineage", &self.lineage)
            .field("view_count", &view_count)
            .finish()
    }
}

impl AnonPrivateObject {
    /// Creates a new root private anonymous object and lineage.
    pub fn new_root() -> Arc<Self> {
        Arc::new(Self {
            id: next_anon_object_id(),
            lineage: next_anon_lineage_id(),
            views: Mutex::new(BTreeMap::new()),
            pages: Mutex::new(BTreeMap::new()),
        })
    }

    /// Creates a child object that belongs to the same anonymous lineage.
    ///
    /// This is the Linux-aligned first step toward expressing fork/COW
    /// families through anonymous object lineage rather than only runtime
    /// state.
    pub fn fork_child(&self) -> Arc<Self> {
        Arc::new(Self {
            id: next_anon_object_id(),
            lineage: self.lineage,
            views: Mutex::new(BTreeMap::new()),
            pages: Mutex::new(BTreeMap::new()),
        })
    }

    /// Creates a new private object that stays within the same lineage after
    /// a COW-style materialization step.
    pub fn cow_child(&self) -> Arc<Self> {
        self.fork_child()
    }

    /// Returns the stable VM object identity.
    pub const fn id(&self) -> VmObjectId {
        VmObjectId::Anon(self.id)
    }

    /// Returns the stable anonymous object identity.
    pub const fn anon_id(&self) -> AnonObjectId {
        self.id
    }

    /// Returns the anonymous lineage identity.
    pub const fn lineage(&self) -> AnonLineageId {
        self.lineage
    }

    /// Registers one private-anonymous VMA view against this object.
    pub fn register_view(self: &Arc<Self>, spec: MappingViewSpec) -> AnonPrivateViewGuard {
        let id = next_mapping_view_id();
        self.views.lock().insert(
            id,
            RegisteredView {
                view: MappingView::new(
                    id,
                    spec.mm_id,
                    MappingViewRange {
                        vma_start: spec.vma_start,
                        vma_len: spec.vma_len,
                        object_start: spec.object_start,
                        object_len: spec.object_len,
                    },
                    spec.kind,
                ),
                notifier: spec.notifier,
            },
        );
        AnonPrivateViewGuard {
            inner: Arc::new(AnonPrivateViewRegistration {
                object: self.clone(),
                id,
            }),
        }
    }

    /// Emits object-side invalidation work for one private-anonymous range.
    pub fn invalidate_range(
        &self,
        object_start: u64,
        object_len: usize,
    ) -> Option<ObjectInvalidateWork> {
        if object_len == 0 {
            return None;
        }
        let mut hits = Vec::new();
        let mut notifiers = Vec::new();
        {
            let views = self.views.lock();
            for registered in views.values() {
                let Some(hit) = registered.view.page_hit(object_start, object_len) else {
                    continue;
                };
                if let Some(notifier) = registered.notifier.as_ref() {
                    notifiers.push((notifier.clone(), hit.clone()));
                }
                hits.push(hit);
            }
        }
        if hits.is_empty() {
            return None;
        }
        let work = ObjectInvalidateWork::new(self.id(), object_start, object_len, hits);
        for (notifier, hit) in notifiers {
            notifier.invalidate(&work, &hit);
        }
        Some(work)
    }

    fn unregister_view(&self, id: MappingViewId) {
        self.views.lock().remove(&id);
    }

    /// Returns one materialized private page slot for the given object offset.
    pub fn page_at(&self, object_start: u64) -> Option<AnonPrivatePageHandle> {
        self.pages
            .lock()
            .get(&object_start)
            .map(|slot| slot.handle.clone())
    }

    /// Prepares a first-touch private page publication.
    ///
    /// Preparation validates that the target object offset is currently empty
    /// but does not publish page ownership. Runtime code uses `commit_after`
    /// to validate typed backing, install the PTE and publish the slot under
    /// one object-map lock. The owning-MM mutex remains the outer exclusion.
    ///
    /// # Errors
    /// Returns `AlreadyExists` when the offset already has a materialized slot.
    pub fn prepare_first_touch_page(
        self: &Arc<Self>,
        object_start: u64,
    ) -> KResult<PreparedAnonPrivatePage> {
        if self.pages.lock().contains_key(&object_start) {
            return Err(KError::AlreadyExists);
        }
        Ok(PreparedAnonPrivatePage {
            object: self.clone(),
            object_start,
        })
    }

    /// Installs one newly materialized private page into this object.
    #[cfg(unittest)]
    fn install_page(
        self: &Arc<Self>,
        object_start: u64,
        page: PageRef,
        size: PageSize,
    ) -> KResult<AnonPrivatePageHandle> {
        self.prepare_first_touch_page(object_start)?
            .commit(page, size)
    }

    /// Replaces one logical slot after comparing its identity and committing a PTE.
    ///
    /// The expected slot stays locked through `commit`. The callback receives
    /// the validated candidate and must acquire mapping holds before mutation,
    /// leave the PTE unchanged on error, and not reenter this map. Success leaves
    /// no fallible work before slot publication. Old-owner content access must
    /// end before the returned retired logical share drops; a physical PTE hold
    /// alone does not prevent premature in-place reuse by a COW sibling.
    ///
    /// # Errors
    /// Returns `InvalidPage` for mismatched geometry, `Changed` if `expected` is
    /// stale, or `Commit` for callback failure. Errors drop the new candidate
    /// outside the map lock and preserve the original slot.
    pub fn replace_page_if_same_after<E>(
        &self,
        object_start: u64,
        expected: &AnonPrivatePageHandle,
        page: PageRef,
        size: PageSize,
        commit: impl FnOnce(&AnonPrivatePageHandle) -> Result<(), E>,
    ) -> Result<RetiredAnonPrivatePage, AnonPrivatePageCommitError<E>> {
        let slot = AnonPrivatePageSlot::new(page, size)
            .map_err(|_| AnonPrivatePageCommitError::InvalidPage)?;
        let old = {
            let mut pages = self.pages.lock();
            let Some(current) = pages.get_mut(&object_start) else {
                return Err(AnonPrivatePageCommitError::Changed);
            };
            if !current.handle.is_same_slot(expected) {
                return Err(AnonPrivatePageCommitError::Changed);
            }
            commit(&slot.handle).map_err(AnonPrivatePageCommitError::Commit)?;
            core::mem::replace(current, slot)
        };
        Ok(RetiredAnonPrivatePage { slot: old })
    }

    /// Removes logical slots after the owning MM has ended their old accesses.
    ///
    /// The caller must hold owning-MM exclusion and complete mapping retirement
    /// or otherwise exclude old-owner access before calling. A failed page-table
    /// walk leaves original slots in place. Removal allocates no temporary
    /// storage and drops each slot outside the map lock.
    /// Physical backing remains protected by every independent `PageRef` hold.
    pub fn remove_range(&self, object_start: u64, object_len: usize) {
        self.retire_range(object_start, object_len, drop);
    }

    /// Removes slots and transfers their logical shares one at a time.
    ///
    /// Hold owning-MM exclusion throughout removal. Each callback runs outside
    /// the map lock and must not repopulate the range. Retain each returned
    /// guard until old-owner accesses end; a physical mapping hold alone cannot
    /// preserve COW isolation. If retirement is unconfirmed, the runtime may
    /// conservatively retain the guard without keeping the object slot visible.
    /// No temporary list, physical release callback or completion state is owned
    /// by this operation.
    pub fn retire_range(
        &self,
        object_start: u64,
        object_len: usize,
        mut retire_fn: impl FnMut(RetiredAnonPrivatePage),
    ) {
        let object_end = object_start.saturating_add(object_len as u64);
        loop {
            let removed = {
                let mut pages = self.pages.lock();
                let Some((&start, _)) = pages.range(object_start..object_end).next() else {
                    break;
                };
                pages.remove(&start)
            };
            if let Some(slot) = removed {
                retire_fn(RetiredAnonPrivatePage { slot });
            }
        }
    }

    /// Reserves logical shares for an unpublished fork child without publication.
    ///
    /// Hold the source owning-MM mutex through source write protection and child
    /// mapping/commit. The child must not be runnable. Rollback must exclude any
    /// child PTE accesses before dropping this preparation. Prepared shares are
    /// owned guards; observers/PTE references never change their count.
    ///
    /// # Errors
    /// Returns `NoMemory` on logical-share overflow, or `AlreadyExists` on a child
    /// slot collision. All reserved guards release outside the object-map locks.
    pub fn prepare_fork_child_pages(
        self: &Arc<Self>,
        object_start: u64,
        object_len: usize,
        child: &Arc<Self>,
    ) -> KResult<PreparedAnonPrivateFork> {
        let object_end = object_start.saturating_add(object_len as u64);
        let mut copied = Vec::new();
        {
            let pages = self.pages.lock();
            for (&start, slot) in pages.range(object_start..object_end) {
                copied.push(AnonPrivateForkPage {
                    object_start: start,
                    slot: slot.try_clone_slot()?,
                });
            }
        }
        {
            let child_pages = child.pages.lock();
            if copied
                .iter()
                .any(|page| child_pages.contains_key(&page.object_start))
            {
                return Err(KError::AlreadyExists);
            }
        }
        Ok(PreparedAnonPrivateFork {
            child: child.clone(),
            pages: copied,
        })
    }
}

unittest::path_mod_test!(tests, "../ut-tests/private.rs");
