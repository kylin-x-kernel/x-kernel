// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn node_kind_assigns_linux_name_and_number() {
    let input = DeviceNodeKind::Input;
    assert_eq!(input.devname(3), "input/event3");
    assert_eq!(input.device_number(3).major(), INPUT_MAJOR);
    assert_eq!(input.device_number(3).minor(), INPUT_EVENT_MINOR_BASE + 3);

    let drm = DeviceNodeKind::Drm;
    assert_eq!(drm.devname(2), "dri/card2");
    assert_eq!(drm.device_number(2).major(), DRM_MAJOR);
    assert_eq!(drm.device_number(2).minor(), 2);
}

#[def_test]
fn class_index_is_reused_only_after_release() {
    let mut registry = DeviceNodeRegistry::new();
    assert_eq!(registry.allocate_index(DeviceNodeKind::Input), Ok(0));
    assert_eq!(registry.allocate_index(DeviceNodeKind::Input), Ok(1));
    registry.free_index(DeviceNodeKind::Input, 0);
    assert_eq!(registry.allocate_index(DeviceNodeKind::Input), Ok(0));
}

#[def_test]
fn removal_keeps_the_old_index_reserved_until_callbacks_finish() {
    let mut registry = DeviceNodeRegistry::new();
    let (original, ..) = registry
        .register(
            DeviceId::new(0),
            DeviceNodeClass::Drm,
            String::from("original"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 0 },
        )
        .expect("original node must fit");
    assert!(registry.finish_publish(&original).is_none());
    assert_eq!(original.index(), 0);

    let (removed, ..) = registry
        .begin_remove(original.core_id())
        .expect("the original node must be removable");
    assert!(!removed.is_available());
    let duplicate = registry.register(
        original.core_id(),
        DeviceNodeClass::Drm,
        String::from("duplicate"),
        String::from("test"),
        DeviceLocation::PlatformStatic { id: 0 },
    );
    assert_eq!(duplicate.err(), Some(DriverError::ResourceBusy));
    let (replacement, ..) = registry
        .register(
            DeviceId::new(1),
            DeviceNodeClass::Drm,
            String::from("replacement"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 1 },
        )
        .expect("replacement node must fit while the old index is reserved");
    assert_eq!(replacement.index(), 1);

    registry.finish_remove(&removed);
    let (later, ..) = registry
        .register(
            DeviceId::new(2),
            DeviceNodeClass::Drm,
            String::from("later"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 2 },
        )
        .expect("the released index must be reusable");
    assert_eq!(later.index(), 0);
}

#[def_test]
fn removal_requested_during_add_preserves_lifecycle_order() {
    let observed = Arc::new(SpinNoPreempt::new(Vec::new()));
    let projection_observed = observed.clone();
    let publisher_observed = observed.clone();
    let mut registry = DeviceNodeRegistry::new();
    registry.subscribers.push(Arc::new(move |event| {
        projection_observed.lock().push(match event {
            DeviceNodeEvent::Added(_) => "projection-add",
            DeviceNodeEvent::Removing(_) => "projection-removing",
            DeviceNodeEvent::Removed(_) => "projection-removed",
        });
    }));
    registry
        .install_lifecycle_publisher(Arc::new(move |event| {
            publisher_observed.lock().push(match event {
                DeviceNodeLifecycleEvent::Added(_) => "publisher-add",
                DeviceNodeLifecycleEvent::Removed(_) => "publisher-remove",
            });
        }))
        .unwrap();

    let (node, subscribers, publisher, is_new) = registry
        .register(
            DeviceId::new(0),
            DeviceNodeClass::Drm,
            String::from("device"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 0 },
        )
        .unwrap();
    assert!(is_new);
    assert!(registry.begin_remove(node.core_id()).is_none());
    assert!(!node.is_available());
    dispatch_node_added(&node, subscribers, publisher);
    let (node, subscribers, publisher) = registry
        .finish_publish(&node)
        .expect("pending removal must follow add publication");
    dispatch_node_removal(&node, subscribers, publisher);
    registry.finish_remove(&node);

    let observed = observed.lock();
    assert_eq!(
        observed.as_slice(),
        [
            "projection-add",
            "publisher-add",
            "projection-removing",
            "publisher-remove",
            "projection-removed",
        ]
    );
}
