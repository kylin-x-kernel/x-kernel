// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;
use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn property_values_cannot_inject_uevent_fields() {
    assert_eq!(
        sanitized_property("keyboard\0ACTION=remove\n"),
        "keyboard ACTION=remove "
    );
}

#[def_test]
fn capability_bitmaps_use_linux_word_order() {
    assert_eq!(bitmap_value(&[1, 0, 0, 0, 2]), "200000001");
    assert_eq!(bitmap_value(&[0]), "0");
    assert_eq!(bitmap_value(&[]), "0");
}

#[def_test]
fn actions_accept_only_the_linux_kobject_set() {
    assert_eq!(UeventAction::try_from("change"), Ok(UeventAction::Change));
    assert_eq!(
        UeventAction::try_from("change\0remove"),
        Err(InvalidUeventAction)
    );
}

#[def_test]
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn drm_render_node_has_its_own_uevent_projection() {
    let device = Arc::new(kdevice::DeviceObject::new_bound_for_test(
        kdevice::DeviceId::new(9001),
        kdevice::DeviceKind::Display,
    ));
    let binding = device.binding_ref().expect("fixture binding");
    let publication = kdevice::PublicationId::allocate().expect("publication id");
    let node = kdevice::register_device_node_for_publication(
        device.id(),
        publication,
        &binding,
        kdevice::DeviceNodeClass::Drm(kdevice::DrmDeviceNodeMetadata::new(
            String::from("Virtual-1"),
            String::from("connected"),
            String::from("On"),
            String::from("1280x800\n"),
            Vec::new(),
        )),
        String::from("display"),
        String::from("test"),
        kdevice::DeviceLocation::PlatformStatic { id: 9001 },
        None,
    )
    .expect("a DRM node must register");
    let render_minor = kdevice::DRM_RENDER_MINOR_BASE + node.index();

    assert_eq!(
        render_uevent_path(&node),
        Some(format!(
            "devices/platform/static9001/drm/renderD{render_minor}"
        ))
    );
    let expected_uevent = format!(
        "MAJOR=226\nMINOR={render_minor}\nDEVNAME=dri/renderD{render_minor}\nDEVTYPE=drm_minor\n"
    );
    assert_eq!(
        uevent_file_contents(&node, UeventScope::Render),
        expected_uevent.into_bytes()
    );
    // The primary `uevent` keeps reporting the primary minor.
    assert_eq!(
        uevent_file_contents(&node, UeventScope::Device),
        format!(
            "MAJOR=226\nMINOR={}\nDEVNAME=dri/card{}\nDEVTYPE=drm_minor\n",
            node.index(),
            node.index()
        )
        .into_bytes()
    );

    assert_eq!(
        kdevice::unregister_device_node_for_publication(
            device.id(),
            publication,
            &binding,
            Box::new(|| {}),
        ),
        kdevice::NodeRemoval::Removed
    );
}
