// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

/// A minimal connector snapshot for a test DRM card.
fn drm_metadata() -> DrmDeviceNodeMetadata {
    DrmDeviceNodeMetadata::new(
        String::from("Virtual-1"),
        String::from("connected"),
        String::from("On"),
        String::from("1280x800\n"),
        Vec::new(),
    )
}

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
fn render_node_derives_from_the_primary_index() {
    let drm = DeviceNodeKind::Drm;
    assert_eq!(drm.devname(0), "dri/card0");

    let mut registry = DeviceNodeRegistry::new();
    // Occupy index 0 first, so the node under test has a non-zero index: with
    // index 0 the assertion could not tell `base + index` from `base`.
    let (_first, ..) = registry
        .register(
            DeviceId::new(6),
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("first"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 6 },
            None,
        )
        .expect("the first DRM node must fit");
    let (node, ..) = registry
        .register(
            DeviceId::new(7),
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("display"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 7 },
            None,
        )
        .expect("a DRM node must fit");
    assert_ne!(node.index(), 0, "the test needs a non-zero card index");
    let render = node
        .render_device_number()
        .expect("DRM nodes have a render minor");
    assert_eq!(render.major(), DRM_MAJOR);
    assert_eq!(render.minor(), DRM_RENDER_MINOR_BASE + node.index());
    assert_eq!(
        node.render_devname(),
        Some(format!(
            "dri/renderD{}",
            DRM_RENDER_MINOR_BASE + node.index()
        ))
    );

    let (input, ..) = registry
        .register(
            DeviceId::new(8),
            None,
            DeviceNodeClass::Input(InputDeviceNodeMetadata::new(
                String::from("keyboard"),
                String::new(),
                String::new(),
                InputDeviceNodeId::new(1, 1, 1, 1),
                Vec::new(),
                BTreeMap::new(),
            )),
            String::from("keyboard"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 8 },
            None,
        )
        .expect("an input node must fit");
    assert_eq!(input.render_device_number(), None);
    assert_eq!(input.render_devname(), None);
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
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("original"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 0 },
            None,
        )
        .expect("original node must fit");
    assert!(registry.finish_publish(&original).is_none());
    assert_eq!(original.index(), 0);

    let removal = registry
        .begin_remove_ownerless(original.core_id())
        .expect("the original node must be removable");
    let Some((removed, ..)) = removal else {
        panic!("a published node must dispatch its removal synchronously");
    };
    assert!(!removed.is_available());
    let duplicate = registry.register(
        original.core_id(),
        None,
        DeviceNodeClass::Drm(drm_metadata()),
        String::from("duplicate"),
        String::from("test"),
        DeviceLocation::PlatformStatic { id: 0 },
        None,
    );
    assert_eq!(duplicate.err(), Some(DriverError::ResourceBusy));
    let (replacement, ..) = registry
        .register(
            DeviceId::new(1),
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("replacement"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 1 },
            None,
        )
        .expect("replacement node must fit while the old index is reserved");
    assert_eq!(replacement.index(), 1);

    registry.finish_remove(&removed, None);
    let (later, ..) = registry
        .register(
            DeviceId::new(2),
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("later"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 2 },
            None,
        )
        .expect("the released index must be reusable");
    assert_eq!(later.index(), 0);
}

/// A node that raced a removal while publishing sits in
/// `RemovePending`/`Removing`; it must keep its device id and class index
/// reserved until the add path has dispatched the removal and called
/// `finish_remove`.
#[def_test]
fn pending_removal_reserves_id_and_index_until_finish_remove() {
    let mut registry = DeviceNodeRegistry::new();
    let (node, subscribers, publisher, is_new) = registry
        .register(
            DeviceId::new(0),
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("pending"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 0 },
            None,
        )
        .expect("the pending node must fit");
    assert!(is_new);
    assert_eq!(node.index(), 0);

    // Removal arrives while the add callbacks are still running: the add path
    // must finish the removal sequence.
    assert!(matches!(
        registry.begin_remove_ownerless(node.core_id()),
        Ok(None)
    ));
    assert!(!node.is_available());

    // `RemovePending` still owns the device id and the DRM index.
    assert_eq!(
        registry
            .register(
                DeviceId::new(0),
                None,
                DeviceNodeClass::Drm(drm_metadata()),
                String::from("same-id"),
                String::from("test"),
                DeviceLocation::PlatformStatic { id: 0 },
                None,
            )
            .err(),
        Some(DriverError::ResourceBusy),
        "a pending removal still reserves its device id"
    );
    let (other, ..) = registry
        .register(
            DeviceId::new(1),
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("other-id"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 1 },
            None,
        )
        .expect("an unrelated device may register while the index is reserved");
    assert_eq!(
        other.index(),
        1,
        "an unrelated device must not take the pending node's class index"
    );

    // Finish the add path: publication moves the node to `Removing`.
    dispatch_node_added(&node, subscribers, publisher);
    let (node, subscribers, publisher) = registry
        .finish_publish(&node)
        .expect("a pending removal follows add publication");
    assert_eq!(node.index(), 0);

    // Removal callbacks run before `finish_remove`; the index is still held.
    dispatch_node_removal(&node, subscribers, publisher);
    let (during, ..) = registry
        .register(
            DeviceId::new(2),
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("during-removal"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 2 },
            None,
        )
        .expect("a new device may register while removal callbacks run");
    assert_eq!(
        during.index(),
        2,
        "the old index stays reserved until finish_remove"
    );

    assert!(registry.finish_remove(&node, None).is_none());
    let (later, ..) = registry
        .register(
            DeviceId::new(3),
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("later"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 3 },
            None,
        )
        .expect("the retired index must be reusable");
    assert_eq!(later.index(), 0, "finish_remove releases the old index");
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
            None,
            DeviceNodeClass::Drm(drm_metadata()),
            String::from("device"),
            String::from("test"),
            DeviceLocation::PlatformStatic { id: 0 },
            None,
        )
        .unwrap();
    assert!(is_new);
    assert!(matches!(
        registry.begin_remove_ownerless(node.core_id()),
        Ok(None)
    ));
    assert!(!node.is_available());
    dispatch_node_added(&node, subscribers, publisher);
    let (node, subscribers, publisher) = registry
        .finish_publish(&node)
        .expect("pending removal must follow add publication");
    dispatch_node_removal(&node, subscribers, publisher);
    assert!(
        registry.finish_remove(&node, None).is_none(),
        "the ownerless pending-removal path registers no deferred retirement"
    );

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

/// A callback that is not registered must be dropped after the node registry
/// guard is released. Its captured probe queries the registry on drop; doing
/// that under the guard would self-deadlock.
#[def_test]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: DeferredNodeRetirement is the production Box<dyn FnOnce()> callback contract"
)]
fn unused_deferred_callback_drops_after_registry_unlock() {
    use alloc::{boxed::Box, sync::Arc};
    use core::sync::atomic::{AtomicBool, Ordering};

    struct DropProbe {
        dropped: Arc<AtomicBool>,
    }
    impl Drop for DropProbe {
        fn drop(&mut self) {
            // Re-entering the registry would deadlock if the callback were
            // dropped while the registry spinlock is held.
            let _ = crate::device_nodes_snapshot();
            self.dropped.store(true, Ordering::SeqCst);
        }
    }

    let device = crate::DeviceObject::new_bound_for_test(
        DeviceId::new(0x9001),
        driver_base::DeviceKind::Char,
    );
    let binding = device.binding_ref().expect("fixture has a binding");
    let node = register_device_node_for_binding(
        device.id(),
        &binding,
        DeviceNodeClass::Drm(drm_metadata()),
        String::from("unused-callback"),
        String::from("test"),
        DeviceLocation::PlatformStatic { id: 0x9001 },
        None,
    )
    .expect("fixture node registers");
    let dropped = Arc::new(AtomicBool::new(false));
    let probe = DropProbe {
        dropped: dropped.clone(),
    };
    let callback: DeferredNodeRetirement = Box::new(move || {
        drop(probe);
    });

    // Synchronous removal does not register the callback, so the captured
    // probe must be dropped by the outer wrapper after the registry guard.
    assert_eq!(
        unregister_device_node_for_binding(node.core_id(), &binding, callback),
        NodeRemoval::Removed
    );
    assert!(
        dropped.load(Ordering::SeqCst),
        "the unused callback was not dropped"
    );
}
