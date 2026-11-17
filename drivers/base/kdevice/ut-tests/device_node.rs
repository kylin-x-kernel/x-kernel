// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

/// A minimal connector snapshot for a test DRM card.
fn node_key(id: u64) -> DeviceNodeKey {
    DeviceNodeKey::new(
        DeviceId::new(id),
        PublicationId::allocate().expect("publication id"),
    )
}

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
            node_key(6),
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
            node_key(7),
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
            node_key(8),
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
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: DeferredNodeRetirement is a production Box callback contract"
)]
fn publication_scoped_nodes_are_independent() {
    let device = crate::DeviceObject::new_bound_for_test(
        DeviceId::new(0x9100),
        driver_base::DeviceKind::Char,
    );
    let binding = device.binding_ref().expect("fixture has a binding");
    let first = PublicationId::allocate().expect("publication id");
    let second = PublicationId::allocate().expect("publication id");
    let first_node = register_device_node_for_publication(
        device.id(),
        first,
        &binding,
        DeviceNodeClass::Drm(drm_metadata()),
        String::from("first"),
        String::from("test"),
        DeviceLocation::PlatformStatic { id: 0x9100 },
        None,
    )
    .expect("first node");
    let second_node = register_device_node_for_publication(
        device.id(),
        second,
        &binding,
        DeviceNodeClass::Drm(drm_metadata()),
        String::from("second"),
        String::from("test"),
        DeviceLocation::PlatformStatic { id: 0x9100 },
        None,
    )
    .expect("second node");

    assert_ne!(first_node.key(), second_node.key());
    assert_eq!(
        unregister_device_node_for_publication(
            device.id(),
            first,
            &binding,
            Box::new(|| {}),
        ),
        NodeRemoval::Removed
    );
    assert!(
        crate::device_nodes_snapshot()
            .iter()
            .any(|node| node.key() == second_node.key())
    );
    assert_eq!(
        unregister_device_node_for_publication(
            device.id(),
            second,
            &binding,
            Box::new(|| {}),
        ),
        NodeRemoval::Removed
    );
}
