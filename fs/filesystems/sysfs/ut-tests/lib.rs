// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, assert_ne, def_test};

use super::*;

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
