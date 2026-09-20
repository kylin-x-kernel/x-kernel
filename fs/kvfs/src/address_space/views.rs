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
}

impl Drop for AddressSpaceViewRegistration {
    fn drop(&mut self) {
        if let Some(address_space) = self.address_space.upgrade() {
            address_space.unregister_view(self.id);
        }
    }
}

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

    pub(super) fn invalidate_mappings_from(&self, object_start: u64) {
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
            object_end = object_end.max(registered.view.object_end());
            hits.push(hit.clone());
            if let Some(notifier) = &registered.notifier {
                notifiers.push((hit, notifier.clone()));
            }
        }
        if hits.is_empty() {
            return;
        }
        let len = usize::try_from(object_end - object_start).unwrap_or(usize::MAX);
        let work = ObjectInvalidateWork::new(self.object_id, object_start, len, hits);
        for (hit, notifier) in notifiers {
            notifier.invalidate(&work, &hit);
        }
    }
}
