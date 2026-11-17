// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::boxed::Box;
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
#[expect(
    clippy::allow_attributes,
    reason = "CG-024-lint-exceptions: Dylint lint is unknown to ordinary Clippy"
)]
#[allow(
    unknown_lints,
    std_box_in_kernel_tests,
    reason = "CG-049-no-std-box-in-kernel-tests: the production CharDeviceImpl/DeferredNodeRetirement API requires a standard Box owner"
)]
fn platform_devices_expose_a_modalias_uevent() {
    // An input node, not a DRM one: class indices are allocated per kind, and a
    // DRM node here would take `card0`, making the DRM test's node `card1` and
    // failing its `card0-Virtual-1` assertion depending on run order. The
    // platform-uevent projection is the same for every kind.
    let device = Arc::new(kdevice::DeviceObject::new_bound_for_test(
        kdevice::DeviceId::new(9003),
        kdevice::DeviceKind::Input,
    ));
    let binding = device.binding_ref().expect("fixture binding");
    let publication = kdevice::PublicationId::allocate().expect("publication id");
    let node = kdevice::register_device_node_for_publication(
        device.id(),
        publication,
        &binding,
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
    nodes.insert(node.key(), projected);
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
fn drm_nodes_project_a_render_minor() {
    let device = Arc::new(kdevice::DeviceObject::new_bound_for_test(
        kdevice::DeviceId::new(9002),
        kdevice::DeviceKind::Display,
    ));
    let binding = device.binding_ref().expect("fixture binding");
    let publication = kdevice::PublicationId::allocate().expect("publication id");
    let node = kdevice::register_device_node_for_publication(
        device.id(),
        publication,
        &binding,
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
    nodes.insert(node.key(), projected);
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

    let card = node.index();
    let connector = format!("class/drm/card{card}-Virtual-1");
    let connector_dir = format!("devices/platform/static9002/drm/card{card}-Virtual-1");
    // The connector is a class entry named after the card and connector.
    assert!(
        tree.contains_key(&connector),
        "the connector class entry must exist"
    );
    // The class entry has to resolve to the connector's own directory.
    let Some(TreeEntry::Symlink(target)) = tree.get(&connector) else {
        panic!("the connector class entry must be a symlink");
    };
    assert_eq!(
        target,
        &format!("../../{connector_dir}"),
        "the class entry must point at the connector directory"
    );
    for attribute in ["status", "dpms", "modes", "edid", "enabled"] {
        assert!(
            tree.contains_key(&format!("{connector_dir}/{attribute}")),
            "{attribute} must be projected"
        );
    }
    // `edid` is binary: the bytes must come back exactly as the device reported
    // them. A text attribute (`status`) is newline-terminated; `edid` is not.
    let Some(TreeEntry::Attribute(_, edid)) = tree.get(&format!("{connector_dir}/edid")) else {
        panic!("edid must be an attribute");
    };
    let expected = drm_metadata().edid().to_vec();
    assert_eq!(
        edid.as_slice(),
        expected.as_slice(),
        "the edid attribute is the raw descriptor"
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

#[def_test]
fn system_projection_follows_present_devices_and_permissions() {
    let cpu0 = SystemDeviceState {
        id: SystemDeviceId::new(SystemClass::Cpu, 0),
        is_present: true,
        is_online: true,
        is_going_offline: false,
        physical_start_bytes: 0,
        size_bytes: 0,
    };
    let cpu1 = SystemDeviceState {
        id: SystemDeviceId::new(SystemClass::Cpu, 1),
        is_present: true,
        is_online: false,
        is_going_offline: false,
        physical_start_bytes: 0,
        size_bytes: 0,
    };
    let memory3 = SystemDeviceState {
        id: SystemDeviceId::new(SystemClass::Memory, 3),
        is_present: true,
        is_online: true,
        is_going_offline: false,
        physical_start_bytes: 3 * 128 * 1024 * 1024,
        size_bytes: 128 * 1024 * 1024,
    };
    let mut projection = DeviceProjection::new();
    projection.systems = alloc::vec![cpu0, cpu1, memory3];
    projection.rebuild_tree();
    let tree = &projection.tree;

    for path in [
        "devices/system/cpu",
        "devices/system/memory",
        "devices/system/cpu/cpu0/online",
        "devices/system/cpu/cpu1/online",
        "devices/system/cpu/cpu1/present",
        "devices/system/cpu/cpu1/possible",
        "devices/system/memory/memory3/phys_index",
        "devices/system/memory/memory3/state",
        "devices/system/memory/memory3/online",
        "devices/system/memory/block_size_bytes",
    ] {
        assert!(tree.contains_key(path), "missing {path}");
    }
    let permission = |path: &str| match &tree[path] {
        TreeEntry::SystemAttribute { permission, .. } => permission.bits(),
        _ => panic!("{path} is not a system attribute"),
    };
    // The system view is read-only; state changes belong to the follow-up
    // hotplug change, not this projection.
    assert_eq!(permission("devices/system/cpu/cpu0/online"), 0o444);
    assert_eq!(permission("devices/system/cpu/cpu1/online"), 0o444);
    assert_eq!(permission("devices/system/cpu/cpu1/present"), 0o444);
    assert_eq!(permission("devices/system/memory/memory3/state"), 0o444);
    assert_eq!(permission("devices/system/memory/memory3/online"), 0o444);
    assert_eq!(permission("devices/system/memory/block_size_bytes"), 0o444);

    projection.systems = alloc::vec![cpu0];
    projection.rebuild_tree();
    assert!(!projection.tree.contains_key("devices/system/cpu/cpu1/online"));
    assert!(!projection.tree.contains_key("devices/system/memory/memory3/online"));
}


#[def_test]
fn system_projection_rebuild_refreshes_the_snapshot() {
    let cpu0 = SystemDeviceState {
        id: SystemDeviceId::new(SystemClass::Cpu, 0),
        is_present: true,
        is_online: true,
        is_going_offline: false,
        physical_start_bytes: 0,
        size_bytes: 0,
    };
    let cpu1 = SystemDeviceState {
        id: SystemDeviceId::new(SystemClass::Cpu, 1),
        is_present: true,
        is_online: false,
        is_going_offline: false,
        physical_start_bytes: 0,
        size_bytes: 0,
    };
    let mut projection = DeviceProjection::new();
    projection.systems = alloc::vec![cpu0, cpu1];
    projection.rebuild_tree();
    assert!(projection.tree.contains_key("devices/system/cpu/cpu1/online"));

    projection.systems = alloc::vec![cpu0];
    projection.rebuild_tree();
    assert!(!projection.tree.contains_key("devices/system/cpu/cpu1/online"));
}
