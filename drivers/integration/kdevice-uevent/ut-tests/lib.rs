// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

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
    assert_eq!(UeventAction::try_from("change\0remove"), Err(()));
}

#[def_test]
fn drm_render_node_has_its_own_uevent_projection() {
    let node = kdevice::register_device_node(
        kdevice::DeviceId::new(9001),
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

    kdevice::unregister_device_node(node.core_id()).expect("legacy node unregisters");
}
