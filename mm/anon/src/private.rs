// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::fmt;

use kerrno::{KError, KResult};
use khal::{mem::PhysAddr, paging::PageSize};
use kspin::SpinNoIrq;
use ksync::Mutex;
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
    pa: PhysAddr,
    size: PageSize,
    refs: u32,
}

/// One object-owned private page shared across one fork/COW family.
#[derive(Clone)]
pub struct AnonPrivatePageHandle {
    inner: Arc<SpinNoIrq<AnonPrivatePageState>>,
}

impl AnonPrivatePageHandle {
    fn new(pa: PhysAddr, size: PageSize) -> Self {
        Self {
            inner: Arc::new(SpinNoIrq::new(AnonPrivatePageState { pa, size, refs: 1 })),
        }
    }

    fn retain(&self) -> KResult {
        let mut state = self.inner.lock();
        state.refs = state.refs.checked_add(1).ok_or(KError::NoMemory)?;
        Ok(())
    }

    fn release(&self) -> Option<AnonPrivateReleasedPage> {
        let mut state = self.inner.lock();
        assert!(state.refs > 0, "dropping unreferenced private page");
        state.refs -= 1;
        if state.refs == 0 {
            Some(AnonPrivateReleasedPage {
                pa: state.pa,
                size: state.size,
            })
        } else {
            None
        }
    }

    /// Returns the backing frame address.
    pub fn phys_addr(&self) -> PhysAddr {
        self.inner.lock().pa
    }

    /// Returns the page size of this private page slot.
    pub fn page_size(&self) -> PageSize {
        self.inner.lock().size
    }

    /// Returns `true` when this page is exclusively owned by one object slot.
    pub fn is_exclusive(&self) -> bool {
        self.inner.lock().refs == 1
    }

    fn is_same_slot(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// One released private page whose last object reference just disappeared.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AnonPrivateReleasedPage {
    pa: PhysAddr,
    size: PageSize,
}

impl AnonPrivateReleasedPage {
    /// Returns the released frame address.
    pub const fn phys_addr(self) -> PhysAddr {
        self.pa
    }

    /// Returns the released page size.
    pub const fn page_size(self) -> PageSize {
        self.size
    }
}

/// One detached private page slot whose final release is deferred until the
/// owning runtime has torn down its visible mappings.
pub struct DetachedAnonPrivatePages {
    object: Arc<AnonPrivateObject>,
    slots: Vec<(u64, AnonPrivatePageHandle)>,
    finalized: bool,
}

impl DetachedAnonPrivatePages {
    /// Returns `true` when no private page slots were detached.
    pub fn is_empty(&self) -> bool {
        self.slots.is_empty()
    }

    /// Finalizes the detached slots and returns frames whose last object
    /// reference disappeared after the caller finished unmapping PTEs.
    pub fn finalize_release(mut self) -> Vec<AnonPrivateReleasedPage> {
        self.finalized = true;
        self.slots
            .drain(..)
            .filter_map(|(_, slot)| slot.release())
            .collect()
    }
}

impl Drop for DetachedAnonPrivatePages {
    fn drop(&mut self) {
        if self.finalized || self.slots.is_empty() {
            return;
        }
        let mut pages = self.object.pages.lock();
        for (object_start, slot) in self.slots.drain(..) {
            let previous = pages.insert(object_start, slot);
            assert!(
                previous.is_none(),
                "rolling back detached private page collided with live slot"
            );
        }
    }
}

/// One page slot copied into a fork child object.
#[derive(Clone)]
pub struct AnonPrivateForkPage {
    object_start: u64,
    handle: AnonPrivatePageHandle,
}

impl AnonPrivateForkPage {
    /// Returns the byte offset inside the anonymous object.
    pub const fn object_start(&self) -> u64 {
        self.object_start
    }

    /// Returns the shared page slot.
    pub fn handle(&self) -> &AnonPrivatePageHandle {
        &self.handle
    }
}

/// Prepared fork sharing state that only commits into the child object after
/// the caller finishes page-table installation.
pub struct PreparedAnonPrivateFork {
    child: Arc<AnonPrivateObject>,
    pages: Vec<AnonPrivateForkPage>,
    committed: bool,
}

impl PreparedAnonPrivateFork {
    /// Returns the prepared shared page slots that should be installed into
    /// the child page table.
    pub fn pages(&self) -> &[AnonPrivateForkPage] {
        &self.pages
    }

    /// Commits the prepared slots into the child object after all page-table
    /// updates succeeded.
    pub fn commit(mut self) -> KResult {
        let mut child_pages = self.child.pages.lock();
        for page in &self.pages {
            if child_pages.contains_key(&page.object_start) {
                return Err(KError::AlreadyExists);
            }
        }
        for page in &self.pages {
            let previous = child_pages.insert(page.object_start, page.handle.clone());
            assert!(
                previous.is_none(),
                "prepared child page collided during commit"
            );
        }
        self.committed = true;
        Ok(())
    }
}

impl Drop for PreparedAnonPrivateFork {
    fn drop(&mut self) {
        if self.committed {
            return;
        }
        for page in self.pages.drain(..) {
            let _ = page.handle.release();
        }
    }
}

/// Prepared first-touch page state that publishes into the anonymous object
/// only after the caller successfully installs the page-table entry.
#[must_use]
pub struct PreparedAnonPrivatePage {
    object: Arc<AnonPrivateObject>,
    object_start: u64,
}

impl PreparedAnonPrivatePage {
    /// Commits the newly materialized page into the owning anonymous object.
    ///
    /// The caller owns the physical frame until this method succeeds. If this
    /// method returns an error, the caller must tear down any visible PTE and
    /// release the frame itself.
    pub fn commit(self, pa: PhysAddr, size: PageSize) -> KResult<AnonPrivatePageHandle> {
        let mut pages = self.object.pages.lock();
        if pages.contains_key(&self.object_start) {
            return Err(KError::AlreadyExists);
        }
        let handle = AnonPrivatePageHandle::new(pa, size);
        pages.insert(self.object_start, handle.clone());
        Ok(handle)
    }
}

/// Error returned while committing a private page slot replacement.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnonPrivatePageCommitError<E> {
    /// The object slot no longer matches the page observed by the caller.
    Changed,
    /// The caller-provided runtime commit step failed.
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

/// Private anonymous object owner.
pub struct AnonPrivateObject {
    id: AnonObjectId,
    lineage: AnonLineageId,
    views: Mutex<BTreeMap<MappingViewId, RegisteredView>>,
    pages: Mutex<BTreeMap<u64, AnonPrivatePageHandle>>,
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
        self.pages.lock().get(&object_start).cloned()
    }

    /// Prepares a first-touch private page publication.
    ///
    /// Preparation validates that the target object offset is currently empty
    /// but does not publish page ownership. Runtime code must install the PTE
    /// first and then call `PreparedAnonPrivatePage::commit()`.
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
        &self,
        object_start: u64,
        pa: PhysAddr,
        size: PageSize,
    ) -> KResult<AnonPrivatePageHandle> {
        let mut pages = self.pages.lock();
        if pages.contains_key(&object_start) {
            return Err(KError::AlreadyExists);
        }
        let handle = AnonPrivatePageHandle::new(pa, size);
        pages.insert(object_start, handle.clone());
        Ok(handle)
    }

    /// Replaces one existing private page with a newly materialized COW page.
    #[cfg(unittest)]
    fn replace_page(
        &self,
        object_start: u64,
        pa: PhysAddr,
        size: PageSize,
    ) -> KResult<Option<AnonPrivateReleasedPage>> {
        let mut pages = self.pages.lock();
        let new_handle = AnonPrivatePageHandle::new(pa, size);
        let old = pages.insert(object_start, new_handle);
        Ok(old.and_then(|slot| slot.release()))
    }

    /// Replaces one private page slot only if it still matches `expected`.
    ///
    /// `commit` is executed while the object slot is locked and still matches
    /// the expected page. Runtime code uses this to perform the page-table
    /// compare/replace step without letting the object slot change between the
    /// object check and object publication.
    pub fn replace_page_if_same_after<E>(
        &self,
        object_start: u64,
        expected: &AnonPrivatePageHandle,
        pa: PhysAddr,
        size: PageSize,
        commit: impl FnOnce() -> Result<(), E>,
    ) -> Result<Option<AnonPrivateReleasedPage>, AnonPrivatePageCommitError<E>> {
        let mut pages = self.pages.lock();
        let Some(current) = pages.get(&object_start) else {
            return Err(AnonPrivatePageCommitError::Changed);
        };
        if !current.is_same_slot(expected) {
            return Err(AnonPrivatePageCommitError::Changed);
        }
        let new_handle = AnonPrivatePageHandle::new(pa, size);
        commit().map_err(AnonPrivatePageCommitError::Commit)?;
        let old = pages
            .insert(object_start, new_handle)
            .expect("validated private page slot must exist");
        Ok(old.release())
    }

    /// Detaches one range of private page slots from this object.
    ///
    /// The caller must first tear down any still-visible PTEs and only then
    /// finalize the detached slots to release frames whose last object
    /// reference disappeared.
    pub fn detach_range(
        self: &Arc<Self>,
        object_start: u64,
        object_len: usize,
    ) -> DetachedAnonPrivatePages {
        if object_len == 0 {
            return DetachedAnonPrivatePages {
                object: self.clone(),
                slots: Vec::new(),
                finalized: true,
            };
        }
        let object_end = object_start.saturating_add(object_len as u64);
        let keys = {
            let pages = self.pages.lock();
            pages
                .range(object_start..object_end)
                .map(|(&key, _)| key)
                .collect::<Vec<_>>()
        };
        let removed = {
            let mut pages = self.pages.lock();
            keys.iter()
                .copied()
                .filter_map(|key| pages.remove(&key))
                .collect::<Vec<_>>()
        };
        DetachedAnonPrivatePages {
            object: self.clone(),
            slots: keys.into_iter().zip(removed).collect(),
            finalized: false,
        }
    }

    /// Prepares to share one object range into a fork child while preserving
    /// lineage, but does not commit child object state until the caller
    /// finishes page-table installation.
    pub fn prepare_fork_child_pages(
        self: &Arc<Self>,
        object_start: u64,
        object_len: usize,
        child: &Arc<Self>,
    ) -> KResult<PreparedAnonPrivateFork> {
        if object_len == 0 {
            return Ok(PreparedAnonPrivateFork {
                child: child.clone(),
                pages: Vec::new(),
                committed: true,
            });
        }
        let object_end = object_start.saturating_add(object_len as u64);
        let slots = {
            let pages = self.pages.lock();
            pages
                .range(object_start..object_end)
                .map(|(&start, handle)| (start, handle.clone()))
                .collect::<Vec<_>>()
        };
        {
            let child_pages = child.pages.lock();
            for (start, _) in &slots {
                if child_pages.contains_key(start) {
                    return Err(KError::AlreadyExists);
                }
            }
        }
        let mut copied: Vec<AnonPrivateForkPage> = Vec::with_capacity(slots.len());
        for (start, handle) in slots {
            if let Err(err) = handle.retain() {
                for page in copied.drain(..) {
                    let _ = page.handle.release();
                }
                return Err(err);
            }
            copied.push(AnonPrivateForkPage {
                object_start: start,
                handle,
            });
        }
        Ok(PreparedAnonPrivateFork {
            child: child.clone(),
            pages: copied,
            committed: false,
        })
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;
    use vmobj::{MappingViewKind, MappingViewSpec};

    use super::{AnonPrivateObject, AnonPrivatePageCommitError};

    #[def_test]
    fn anon_private_fork_child_keeps_lineage_and_gets_new_identity() {
        let parent = AnonPrivateObject::new_root();
        let child = parent.fork_child();

        assert_eq!(parent.lineage(), child.lineage());
        assert_ne!(parent.id(), child.id());
        assert_ne!(parent.anon_id(), child.anon_id());
    }

    #[def_test]
    fn anon_private_registers_views_and_emits_object_hits() {
        let object = AnonPrivateObject::new_root();
        let _view = object.register_view(MappingViewSpec {
            mm_id: 9,
            vma_start: 0x9000,
            vma_len: 0x3000,
            object_start: 0x0,
            object_len: 0x3000,
            kind: MappingViewKind::Private,
            notifier: None,
        });

        let work = object
            .invalidate_range(0x1000, 0x1000)
            .expect("private anon object hit must be reported");
        assert_eq!(work.hits().len(), 1);
        let hit = &work.hits()[0];
        assert_eq!(hit.view().mm_id(), 9);
        assert_eq!(hit.vma_start(), 0xa000);
        assert_eq!(hit.object_start(), 0x1000);
        assert_eq!(hit.object_len(), 0x1000);
    }

    #[def_test]
    fn anon_private_page_state_fork_and_discard_work() {
        use khal::{mem::PhysAddr, paging::PageSize};

        let parent = AnonPrivateObject::new_root();
        let child = parent.fork_child();
        let handle = parent
            .install_page(0x1000, PhysAddr::from_usize(0x2000), PageSize::Size4K)
            .expect("install private page");
        assert!(handle.is_exclusive());

        let prepared = parent
            .prepare_fork_child_pages(0x1000, 0x1000, &child)
            .expect("prepare fork child pages");
        assert_eq!(prepared.pages().len(), 1);
        assert_eq!(prepared.pages()[0].object_start(), 0x1000);
        assert!(!handle.is_exclusive());
        prepared.commit().expect("commit fork child pages");
        assert_eq!(
            child.page_at(0x1000).expect("child page").phys_addr(),
            PhysAddr::from_usize(0x2000)
        );

        let released = parent.detach_range(0x1000, 0x1000).finalize_release();
        assert!(released.is_empty());
        let released = child.detach_range(0x1000, 0x1000).finalize_release();
        assert_eq!(released.len(), 1);
        assert_eq!(released[0].phys_addr(), PhysAddr::from_usize(0x2000));
    }

    #[def_test]
    fn anon_private_first_touch_prepare_commits_page_slot() {
        use khal::{mem::PhysAddr, paging::PageSize};

        let object = AnonPrivateObject::new_root();
        let prepared = object
            .prepare_first_touch_page(0x1000)
            .expect("prepare first-touch page");

        assert!(
            object.page_at(0x1000).is_none(),
            "prepared first-touch page must not publish object state"
        );

        let handle = prepared
            .commit(PhysAddr::from_usize(0x5000), PageSize::Size4K)
            .expect("commit first-touch page");
        assert_eq!(handle.phys_addr(), PhysAddr::from_usize(0x5000));
        assert_eq!(
            object
                .page_at(0x1000)
                .expect("committed page slot")
                .phys_addr(),
            PhysAddr::from_usize(0x5000)
        );
    }

    #[def_test]
    fn anon_private_first_touch_commit_revalidates_empty_slot() {
        use kerrno::KError;
        use khal::{mem::PhysAddr, paging::PageSize};

        let object = AnonPrivateObject::new_root();
        let prepared = object
            .prepare_first_touch_page(0x1000)
            .expect("prepare first-touch page");
        object
            .install_page(0x1000, PhysAddr::from_usize(0x6000), PageSize::Size4K)
            .expect("install competing page");

        let err = match prepared.commit(PhysAddr::from_usize(0x7000), PageSize::Size4K) {
            Ok(_) => panic!("commit must reject a slot that became populated"),
            Err(err) => err,
        };
        assert_eq!(err, KError::AlreadyExists);
        assert_eq!(
            object
                .page_at(0x1000)
                .expect("original page must remain installed")
                .phys_addr(),
            PhysAddr::from_usize(0x6000)
        );
    }

    #[def_test]
    fn anon_private_replace_if_same_rejects_changed_slot() {
        use khal::{mem::PhysAddr, paging::PageSize};

        let object = AnonPrivateObject::new_root();
        let expected = object
            .install_page(0x1000, PhysAddr::from_usize(0x6000), PageSize::Size4K)
            .expect("install expected page");
        object
            .replace_page(0x1000, PhysAddr::from_usize(0x7000), PageSize::Size4K)
            .expect("replace competing page");

        let result = object.replace_page_if_same_after(
            0x1000,
            &expected,
            PhysAddr::from_usize(0x8000),
            PageSize::Size4K,
            || Ok::<_, ()>(()),
        );
        assert!(matches!(result, Err(AnonPrivatePageCommitError::Changed)));
        assert_eq!(
            object
                .page_at(0x1000)
                .expect("competing page must remain installed")
                .phys_addr(),
            PhysAddr::from_usize(0x7000)
        );
    }

    #[def_test]
    fn anon_private_detach_rolls_back_when_not_finalized() {
        use khal::{mem::PhysAddr, paging::PageSize};

        let object = AnonPrivateObject::new_root();
        object
            .install_page(0x1000, PhysAddr::from_usize(0x3000), PageSize::Size4K)
            .expect("install private page");

        let detached = object.detach_range(0x1000, 0x1000);
        assert!(object.page_at(0x1000).is_none());
        drop(detached);

        let page = object
            .page_at(0x1000)
            .expect("detached page must roll back");
        assert_eq!(page.phys_addr(), PhysAddr::from_usize(0x3000));
    }

    #[def_test]
    fn anon_private_fork_prepare_rolls_back_without_commit() {
        use khal::{mem::PhysAddr, paging::PageSize};

        let parent = AnonPrivateObject::new_root();
        let child = parent.fork_child();
        let handle = parent
            .install_page(0x1000, PhysAddr::from_usize(0x4000), PageSize::Size4K)
            .expect("install private page");

        {
            let prepared = parent
                .prepare_fork_child_pages(0x1000, 0x1000, &child)
                .expect("prepare fork pages");
            assert_eq!(prepared.pages().len(), 1);
            assert!(!handle.is_exclusive());
        }

        assert!(handle.is_exclusive());
        assert!(child.page_at(0x1000).is_none());
    }
}
