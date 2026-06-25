// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc, vec::Vec};
use core::fmt;

use ksync::Mutex;
use vmobj::{
    AnonObjectId, MappingView, MappingViewId, MappingViewNotifier, MappingViewRange,
    MappingViewSpec, ObjectInvalidateWork, VmObjectId, next_mapping_view_id,
};

use crate::ids::next_anon_object_id;

struct RegisteredView {
    view: MappingView,
    notifier: Option<Arc<dyn MappingViewNotifier>>,
}

struct AnonSharedViewRegistration {
    object: Arc<AnonSharedObject>,
    id: MappingViewId,
}

/// Lifetime guard for one registered anonymous-shared view.
#[derive(Clone)]
pub struct AnonSharedViewGuard {
    inner: Arc<AnonSharedViewRegistration>,
}

impl AnonSharedViewGuard {
    /// Returns the stable registration id kept alive by this guard.
    pub fn id(&self) -> MappingViewId {
        self.inner.id
    }
}

impl Drop for AnonSharedViewRegistration {
    fn drop(&mut self) {
        self.object.unregister_view(self.id);
    }
}

/// Shared anonymous object owner.
pub struct AnonSharedObject {
    id: AnonObjectId,
    views: Mutex<BTreeMap<MappingViewId, RegisteredView>>,
}

impl fmt::Debug for AnonSharedObject {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let view_count = self.views.lock().len();
        f.debug_struct("AnonSharedObject")
            .field("id", &self.id)
            .field("view_count", &view_count)
            .finish()
    }
}

impl AnonSharedObject {
    /// Creates a fresh shared anonymous object.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            id: next_anon_object_id(),
            views: Mutex::new(BTreeMap::new()),
        })
    }

    /// Returns the stable VM object identity.
    pub const fn id(&self) -> VmObjectId {
        VmObjectId::Anon(self.id)
    }

    /// Returns the stable anonymous object identity.
    pub const fn anon_id(&self) -> AnonObjectId {
        self.id
    }

    /// Registers one shared-anonymous VMA view against this object.
    pub fn register_view(self: &Arc<Self>, spec: MappingViewSpec) -> AnonSharedViewGuard {
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
        AnonSharedViewGuard {
            inner: Arc::new(AnonSharedViewRegistration {
                object: self.clone(),
                id,
            }),
        }
    }

    /// Emits object-side invalidation work for one anonymous shared byte range.
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
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;
    use vmobj::{MappingViewKind, MappingViewSpec, VmObjectId};

    use super::AnonSharedObject;
    use crate::AnonPrivateObject;

    #[def_test]
    fn anon_objects_use_typed_anon_identity() {
        let shared = AnonSharedObject::new();
        let private = AnonPrivateObject::new_root();

        assert!(matches!(shared.id(), VmObjectId::Anon(_)));
        assert!(matches!(private.id(), VmObjectId::Anon(_)));
        assert_ne!(shared.id(), private.id());
    }

    #[def_test]
    fn anon_shared_registers_views_and_emits_object_hits() {
        let object = AnonSharedObject::new();
        let _view = object.register_view(MappingViewSpec {
            mm_id: 7,
            vma_start: 0x4000,
            vma_len: 0x3000,
            object_start: 0x0,
            object_len: 0x3000,
            kind: MappingViewKind::Shared,
            notifier: None,
        });

        let work = object
            .invalidate_range(0x1000, 0x1000)
            .expect("shared anon object hit must be reported");
        assert_eq!(work.hits().len(), 1);
        let hit = &work.hits()[0];
        assert_eq!(hit.view().mm_id(), 7);
        assert_eq!(hit.vma_start(), 0x5000);
        assert_eq!(hit.object_start(), 0x1000);
        assert_eq!(hit.object_len(), 0x1000);
    }
}
