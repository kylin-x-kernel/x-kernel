// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Mapped-view registration and invalidation notifications.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use vmobj::{
    MappingView, MappingViewId, MappingViewNotifier, MappingViewRange, MappingViewSpec,
    ObjectInvalidateWork, next_mapping_view_id,
};

use super::AddressSpace;

struct AddressSpaceViewRegistration {
    address_space: Weak<AddressSpace>,
    id: MappingViewId,
}

/// Lifetime guard for a VMA registered in an inode address space.
#[derive(Clone)]
pub struct AddressSpaceViewGuard {
    inner: Arc<AddressSpaceViewRegistration>,
}

impl AddressSpaceViewGuard {
    /// Returns the stable registration id kept alive by this guard.
    pub fn id(&self) -> MappingViewId {
        self.inner.id
    }

    /// Extends this registration to a new VMA end while MM serialization is held.
    ///
    /// The object and VMA spans grow together; identity and notifier are retained.
    ///
    /// # Errors
    /// Returns InvalidInput for overflowing coordinates, or BadState if detached.
    pub fn extend_to(&self, end: u64) -> crate::VfsResult<()> {
        let mapping = self
            .inner
            .address_space
            .upgrade()
            .ok_or(crate::VfsError::BadState)?;
        let mut views = mapping.views.lock();
        let registered = views
            .get_mut(&self.inner.id)
            .ok_or(crate::VfsError::BadState)?;
        let view = &registered.view;
        if end <= view.vma_end() {
            return Ok(());
        }
        let len =
            usize::try_from(end - view.vma_start()).map_err(|_| crate::VfsError::InvalidInput)?;
        let range = MappingViewRange {
            vma_len: len,
            object_len: len,
            ..view.range()
        };
        registered.view = MappingView::try_new(view.id(), view.mm_id(), range, view.kind())
            .ok_or(crate::VfsError::InvalidInput)?;
        Ok(())
    }
}

impl Drop for AddressSpaceViewRegistration {
    fn drop(&mut self) {
        if let Some(address_space) = self.address_space.upgrade() {
            address_space.unregister_view(self.id);
        }
    }
}

type ViewNotifiers = Vec<Option<Arc<dyn MappingViewNotifier>>>;

pub(super) struct RegisteredView {
    view: MappingView,
    notifier: Option<Arc<dyn MappingViewNotifier>>,
}

impl AddressSpace {
    /// Registers a VMA view of this inode address space.
    pub fn register_view(self: &Arc<Self>, spec: MappingViewSpec) -> AddressSpaceViewGuard {
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
        AddressSpaceViewGuard {
            inner: Arc::new(AddressSpaceViewRegistration {
                address_space: Arc::downgrade(self),
                id,
            }),
        }
    }

    fn unregister_view(&self, id: MappingViewId) {
        self.views.lock().remove(&id);
    }

    /// Returns the object ranges of all registered views, for tests.
    #[cfg(unittest)]
    pub fn registered_object_ranges(&self) -> alloc::vec::Vec<(u64, u64)> {
        self.views
            .lock()
            .values()
            .map(|registered| (registered.view.object_start(), registered.view.object_end()))
            .collect()
    }

    fn invalidation_work_range(
        &self,
        start: u64,
        end: Option<u64>,
    ) -> Option<(ObjectInvalidateWork, ViewNotifiers)> {
        let page = memaddr::PAGE_SIZE_4K as u64;
        let aligned_start = start - start % page;
        // Round outward so partial leading/trailing pages are unmapped whole,
        // matching the folio discard rounding and Linux page-indexed hooks.
        // Callers pass `start < end` when `end` is `Some`; an inverted range is
        // rejected at the entry before reaching here.
        let aligned_end = end.map(|end| end.div_ceil(page).saturating_mul(page));
        let mut hits = Vec::new();
        let mut notifiers = Vec::new();
        let mut effective_end = aligned_start;
        for registered in self.views.lock().values() {
            if registered.view.object_end() <= aligned_start
                || aligned_end.is_some_and(|end| registered.view.object_start() >= end)
            {
                continue;
            }
            let len = match aligned_end {
                Some(end) => usize::try_from(end - aligned_start).unwrap_or(usize::MAX),
                None => usize::try_from(registered.view.object_end() - aligned_start)
                    .unwrap_or(usize::MAX),
            };
            let Some(hit) = registered.view.page_hit(aligned_start, len) else {
                continue;
            };
            effective_end = effective_end.max(hit.object_end());
            hits.push(hit);
            notifiers.push(registered.notifier.clone());
        }
        if hits.is_empty() {
            return None;
        }
        let object_end = aligned_end.unwrap_or(effective_end);
        let len = usize::try_from(object_end - aligned_start).unwrap_or(usize::MAX);
        let work = ObjectInvalidateWork::new(self.object_id, aligned_start, len, hits);
        Some((work, notifiers))
    }

    pub(super) fn invalidate_mappings_from(&self, object_start: u64) {
        if let Some((work, notifiers)) = self.invalidation_work_range(object_start, None) {
            for (hit, notifier) in work.hits().iter().zip(notifiers) {
                if let Some(notifier) = notifier {
                    notifier.invalidate(&work, hit);
                }
            }
        }
    }

    /// Unmaps every registered view intersecting the byte range starting at
    /// `start` and reports whether every intersect was applied synchronously.
    ///
    /// - `start`: inclusive object byte offset, rounded down to a page boundary.
    /// - `end`: exclusive object byte offset, rounded up to a page boundary;
    ///   `None` extends the range to the end of every intersecting view.
    ///
    /// Returns: `true` when every hit completed PTE removal and TLB
    /// invalidation before returning, or when no view intersects the range.
    /// `false` when at least one target address space was busy and its request
    /// could not be applied synchronously; such a view keeps its PTEs and the
    /// caller must not discard the covered folios.
    ///
    /// Errors: `OperationNotSupported` when an intersecting view has no
    /// notifier; other errors propagate unchanged.
    ///
    /// Locking: takes the views lock briefly and, per view, attempts the target
    /// address-space lock without waiting. Must not be called while holding that
    /// address-space lock. May spin while a remote TLB shootdown completes.
    pub(super) fn try_invalidate_mappings_range(
        &self,
        start: u64,
        end: Option<u64>,
    ) -> crate::VfsResult<bool> {
        if let Some((work, notifiers)) = self.invalidation_work_range(start, end) {
            for (hit, notifier) in work.hits().iter().zip(notifiers) {
                let notifier = notifier.ok_or(crate::VfsError::OperationNotSupported)?;
                if !notifier.try_invalidate(&work, hit)? {
                    return Ok(false);
                }
            }
        }
        Ok(true)
    }
}
