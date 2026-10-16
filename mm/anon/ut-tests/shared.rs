// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
