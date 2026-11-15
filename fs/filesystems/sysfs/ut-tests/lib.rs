// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, assert_ne, def_test};

use super::*;

/// A minimal connector snapshot for a test DRM card.
fn drm_metadata() -> kdevice::DrmDeviceNodeMetadata {
    kdevice::DrmDeviceNodeMetadata::new(
        String::from("Virtual-1"),
        String::from("connected"),
        String::from("On"),
        String::from("1280x800\n"),
        // A recognisable prefix: the EDID header every descriptor starts with,
        // so a projection that rewrites the bytes is caught.
        alloc::vec![0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x00],
    )
}

#[def_test]
fn pci_paths_are_derived_from_bus_location() {
    let path = kdevice_uevent::physical_device_path(DeviceLocation::Pci {
        segment: 0,
        bus: 4,
        device: 2,
        function: 1,
    });
    assert_eq!(path, "pci0000:04/0000:04:02.1");
    assert_ne!(path.as_str(), "platform/simpledrm");
}

#[def_test]
fn capability_bitmaps_use_linux_word_order() {
    assert_eq!(bitmap_text(&[1, 0, 0, 0, 2]), b"200000001\n");
    assert_eq!(bitmap_text(&[0]), b"0\n");
    assert_eq!(bitmap_text(&[]), b"0\n");
}

#[def_test]
fn tree_changes_keep_static_mountpoint_ancestors_stable() {
    let mut previous = BTreeMap::new();
    previous.insert(
        "devices".to_string(),
        TreeEntry::Directory(Arc::new(DirectoryEntry::new())),
    );
    previous.insert(
        "fs".to_string(),
        TreeEntry::Directory(Arc::new(DirectoryEntry::new())),
    );
    previous.insert(
        "fs/cgroup".to_string(),
        TreeEntry::Directory(Arc::new(DirectoryEntry::new())),
    );

    let mut current = previous.clone();
    current.insert(
        "devices/platform".to_string(),
        TreeEntry::Directory(Arc::new(DirectoryEntry::new())),
    );
    current.insert(
        "devices/platform/device0".to_string(),
        TreeEntry::Directory(Arc::new(DirectoryEntry::new())),
    );

    assert_eq!(
        changed_paths(&previous, &current),
        alloc::vec!["devices/platform".to_string()]
    );
}

#[def_test]
fn rebuild_reuses_live_directory_inode_owner() {
    let mut projection = DeviceProjection::new();
    let TreeEntry::Directory(before) = projection.tree.get("fs/cgroup").unwrap() else {
        panic!("cgroup mountpoint must be a directory");
    };
    let before = before.clone();

    assert_eq!(projection.rebuild_tree(), Vec::<String>::new());
    let TreeEntry::Directory(after) = projection.tree.get("fs/cgroup").unwrap() else {
        panic!("cgroup mountpoint must remain a directory");
    };
    assert!(Arc::ptr_eq(&before, after));
}

#[def_test]
fn virtual_character_projection_tracks_registration_and_retirement() {
    struct Backend;
    impl kvfs::DeviceFileOps for Backend {}
    init_device_projection();
    let device = kvfs::DeviceId::new(245, 900001);
    let registration = kvfs::cdev_add_named(
        device,
        String::from("projection_test"),
        String::from("registration_test"),
        Arc::new(Backend),
    )
    .unwrap();
    let path = "devices/virtual/registration_test/projection_test/dev";
    let entry = device_projection().lock().tree.get(path).cloned().unwrap();
    let TreeEntry::CharacterAttribute(identity, data) = entry else {
        panic!("named cdev must have an attribute projection");
    };
    assert!(identity.same_registration(&registration));
    assert_eq!(data, b"245:900001\n");
    assert!(
        device_projection()
            .lock()
            .tree
            .contains_key("class/registration_test/projection_test")
    );
    kvfs::cdev_del(device, 1).unwrap();
    assert!(!identity.is_registered());
    assert!(!device_projection().lock().tree.contains_key(path));
    assert!(!build_tree(&BTreeMap::new(), &[registration]).contains_key(path));
}

#[def_test]
fn symlink_targets_are_relative_to_the_link_parent() {
    assert_eq!(
        relative_symlink(
            "class/drm/card0",
            "devices/pci0000:00/0000:00:03.0/drm/card0"
        ),
        "../../devices/pci0000:00/0000:00:03.0/drm/card0"
    );
    assert_eq!(
        relative_symlink(
            "devices/pci0000:00/0000:00:03.0/drm/card0/device",
            "devices/pci0000:00/0000:00:03.0"
        ),
        "../.."
    );
}

#[def_test]
fn platform_devices_expose_a_modalias_uevent() {
    // An input node, not a DRM one: class indices are allocated per kind, and a
    // DRM node here would take `card0`, making the DRM test's node `card1` and
    // failing its `card0-Virtual-1` assertion depending on run order. The
    // platform-uevent projection is the same for every kind.
    let node = kdevice::register_device_node(
        kdevice::DeviceId::new(9003),
        kdevice::DeviceNodeClass::Input(kdevice::InputDeviceNodeMetadata::new(
            String::from("test-input"),
            String::from("test"),
            String::from("test"),
            kdevice::InputDeviceNodeId {
                bus_type: 0x19,
                vendor: 1,
                product: 2,
                version: 3,
            },
            alloc::vec![],
            BTreeMap::new(),
        )),
        String::from("virtio-input-mmio"),
        String::from("test"),
        kdevice::DeviceLocation::Mmio {
            base: 0xa003e00,
            size: 0x1000,
        },
        None,
    )
    .expect("an MMIO device node must register");
    let projected = Arc::new(ProjectedDevice::new(node.clone()));
    let mut nodes = BTreeMap::new();
    nodes.insert(node.core_id(), projected);
    let tree = build_tree(&nodes, &[]);

    // libdrm's `drmParseOFBusInfo` and `drmParseOFDeviceInfo` read
    // `OF_FULLNAME`/`OF_COMPATIBLE_N` first and fall back to `MODALIAS` on the
    // device directory's `uevent`. Without either attribute `drmGetDevices2`
    // drops the platform node and clients report "no drm device found".
    let path = "devices/platform/a003e00.mmio/uevent";
    let Some(TreeEntry::RawUevent { contents, .. }) = tree.get(path) else {
        panic!("a platform device must expose a device-directory uevent");
    };
    let text = core::str::from_utf8(contents).expect("uevent is text");
    assert!(
        text.contains("MODALIAS=platform:a003e00.mmio\n"),
        "the uevent must carry a platform MODALIAS, got {text:?}"
    );
    assert!(
        text.contains("DRIVER=test\n"),
        "the uevent must name the bound driver, got {text:?}"
    );

    // Release the node so a later test's `build_tree` does not see it.
    kdevice::unregister_device_node(node.core_id()).expect("test node unregisters");
}

#[def_test]
fn drm_nodes_project_a_render_minor() {
    let node = kdevice::register_device_node(
        kdevice::DeviceId::new(9002),
        kdevice::DeviceNodeClass::Drm(drm_metadata()),
        String::from("display"),
        String::from("test"),
        kdevice::DeviceLocation::PlatformStatic { id: 9002 },
        None,
    )
    .expect("a DRM node must register");
    let minor = kdevice::DRM_RENDER_MINOR_BASE + node.index();
    let projected = Arc::new(ProjectedDevice::new(node.clone()));
    let mut nodes = BTreeMap::new();
    nodes.insert(node.core_id(), projected);
    let tree = build_tree(&nodes, &[]);

    let render = format!("devices/platform/static9002/drm/renderD{minor}");
    assert!(tree.contains_key(&render));
    assert!(tree.contains_key(&format!("class/drm/renderD{minor}")));
    assert!(tree.contains_key(&format!("dev/char/226:{minor}")));
    let uevent_path = format!("{render}/uevent");
    let Some(TreeEntry::Uevent(_, UeventScope::Render)) = tree.get(&uevent_path) else {
        panic!("render nodes must expose a writable uevent attribute");
    };

    assert!(tree.contains_key(&format!("dev/char/226:{}", node.index())));

    // The connector is a class entry named after the card (`card0-Virtual-1`)
    // and its attributes are the connector's state.
    assert!(
        tree.contains_key("class/drm/card0-Virtual-1"),
        "the connector class entry must exist"
    );
    // The class entry has to resolve to the connector's own directory: linking
    // it to the card would make every attribute unreadable through the link,
    // which is exactly what `cat /sys/class/drm/card0-Virtual-1/status` does.
    let Some(TreeEntry::Symlink(target)) = tree.get("class/drm/card0-Virtual-1") else {
        panic!("the connector class entry must be a symlink");
    };
    assert_eq!(
        target, "../../devices/platform/static9002/drm/card0-Virtual-1",
        "the class entry must point at the connector directory"
    );
    for attribute in ["status", "dpms", "modes", "edid", "enabled"] {
        assert!(
            tree.contains_key(&format!(
                "devices/platform/static9002/drm/card0-Virtual-1/{attribute}"
            )),
            "{attribute} must be projected"
        );
    }
    // `edid` is binary: the bytes must come back exactly as the device reported
    // them. A text attribute (`status`) is newline-terminated; `edid` is not.
    let Some(TreeEntry::Attribute(_, edid)) =
        tree.get("devices/platform/static9002/drm/card0-Virtual-1/edid")
    else {
        panic!("edid must be an attribute");
    };
    let expected = drm_metadata().edid().to_vec();
    assert_eq!(
        edid.as_slice(),
        expected.as_slice(),
        "the edid attribute is the raw descriptor"
    );

    kdevice::unregister_device_node(node.core_id()).expect("legacy node unregisters");
}

/// `kobject_action_type` takes the first space-separated token after dropping
/// one trailing newline or NUL, and refuses every other whitespace.
#[def_test]
fn uevent_action_is_the_first_space_separated_token() {
    assert_eq!(parse_uevent_action(b"add"), Ok(UeventAction::Add));
    assert_eq!(parse_uevent_action(b"add\n"), Ok(UeventAction::Add));
    assert_eq!(parse_uevent_action(b"add\0"), Ok(UeventAction::Add));
    assert_eq!(parse_uevent_action(b"add uuid-1234"), Ok(UeventAction::Add));
    assert_eq!(parse_uevent_action(b"add  uuid-1234"), Ok(UeventAction::Add));
    assert_eq!(parse_uevent_action(b"change\n"), Ok(UeventAction::Change));

    // A tab, an embedded newline and a leading tab are not separators: the
    // token then carries them and no action matches, so the write is refused.
    assert_eq!(parse_uevent_action(b"add\tuuid-1234"), Err(VfsError::InvalidInput));
    assert_eq!(parse_uevent_action(b"add\nuuid-1234"), Err(VfsError::InvalidInput));
    assert_eq!(parse_uevent_action(b"\tadd"), Err(VfsError::InvalidInput));
    assert_eq!(parse_uevent_action(b""), Err(VfsError::InvalidInput));
    assert_eq!(parse_uevent_action(b"\n"), Err(VfsError::InvalidInput));
}

/// logind's `seat_has_vts()` opens `/sys/class/tty/tty0/active`, so the alias
/// must exist for the `tty`/`tty0` registration and carry the active console.
#[def_test]
fn tty0_projects_the_active_console_alias() {
    init_device_projection();
    let devices = kvfs::registered_character_devices();
    let tty0 = devices
        .iter()
        .find(|registration| registration.class() == "tty" && registration.name() == "tty0")
        .expect("the fixed VT console registers tty0");

    let tree = build_tree(&BTreeMap::new(), core::slice::from_ref(tty0));
    let path = "devices/virtual/tty/tty0/active";
    let Some(TreeEntry::CharacterAttribute(identity, contents)) = tree.get(path) else {
        panic!("tty0 must expose the `active` alias");
    };
    assert!(identity.same_registration(tty0));
    assert_eq!(contents, b"tty1\n", "the fixed VT console is VT 1");
    assert!(
        tree.contains_key("class/tty/tty0"),
        "logind opens the alias through /sys/class"
    );
}
