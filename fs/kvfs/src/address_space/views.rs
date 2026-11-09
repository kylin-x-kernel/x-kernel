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

    fn invalidation_work_from(
        &self,
        object_start: u64,
    ) -> Option<(ObjectInvalidateWork, ViewNotifiers)> {
        let mut hits = Vec::new();
        let mut notifiers = Vec::new();
        let mut object_end = object_start;
        for registered in self.views.lock().values() {
            if registered.view.object_end() <= object_start {
                continue;
            }
            let len =
                usize::try_from(registered.view.object_end() - object_start).unwrap_or(usize::MAX);
            let Some(hit) = registered.view.page_hit(object_start, len) else {
                continue;
            };
            object_end = object_end.max(hit.object_end());
            hits.push(hit);
            notifiers.push(registered.notifier.clone());
        }
        if hits.is_empty() {
            return None;
        }
        let len = usize::try_from(object_end - object_start).unwrap_or(usize::MAX);
        let work = ObjectInvalidateWork::new(self.object_id, object_start, len, hits);
        Some((work, notifiers))
    }

    pub(super) fn invalidate_mappings_from(&self, object_start: u64) {
        if let Some((work, notifiers)) = self.invalidation_work_from(object_start) {
            for (hit, notifier) in work.hits().iter().zip(notifiers) {
                if let Some(notifier) = notifier {
                    notifier.invalidate(&work, hit);
                }
            }
        }
    }

    pub(super) fn try_invalidate_mappings_from(&self, object_start: u64) -> crate::VfsResult<bool> {
        if let Some((work, notifiers)) = self.invalidation_work_from(object_start) {
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
