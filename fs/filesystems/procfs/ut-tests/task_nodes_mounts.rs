// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, sync::Arc};

use kvfs::{DirMapping, Mount, MountFlags, SimpleDir, SimpleFs};
use unittest::{assert_eq, def_test};

use super::ProcMountEntry;

static TEST_FILE_SYSTEM_TYPE: kvfs::FileSystemType = kvfs::FileSystemType::internal("test");

#[def_test]
fn test_format_mount_options_defaults_to_rw() {
    assert_eq!(
        super::format_mount_options(MountFlags::empty(), false),
        "rw"
    );
}

#[def_test]
fn test_format_ro_uses_supplied_readonly_state() {
    assert_eq!(super::format_mount_options(MountFlags::RDONLY, true), "ro");
    assert_eq!(super::format_mount_options(MountFlags::empty(), true), "ro");
    assert_eq!(super::format_mount_options(MountFlags::RDONLY, false), "rw");
    assert_eq!(
        super::format_mount_options(MountFlags::empty(), false),
        "rw"
    );
}

#[def_test]
fn test_format_mount_options_shows_per_mount_flags() {
    let mnt = MountFlags::NOSUID
        | MountFlags::NODEV
        | MountFlags::NOEXEC
        | MountFlags::NOATIME
        | MountFlags::RELATIME;
    let opts = super::format_mount_options(mnt, false);
    assert!(opts.contains("nosuid"));
    assert!(opts.contains("nodev"));
    assert!(opts.contains("noexec"));
    assert!(opts.contains("noatime"));
    assert!(opts.contains("relatime"));
    assert!(!opts.contains("nodiratime"));
    assert!(!opts.contains("nosymfollow"));
}

#[def_test]
fn test_format_mount_options_includes_nodiratime_and_nosymfollow() {
    let mnt = MountFlags::NODIRATIME | MountFlags::NOSYMFOLLOW;
    let opts = super::format_mount_options(mnt, false);
    assert!(opts.contains("nodiratime"));
    assert!(opts.contains("nosymfollow"));
}

#[def_test]
fn test_mounts_uses_effective_readonly_options() {
    let entry = ProcMountEntry {
        mount_id: 1,
        parent_id: 0,
        major: 0,
        minor: 1,
        root: "/".into(),
        mount_point: "/".into(),
        fs_type: "ext4".into(),
        source: "rootfs".into(),
        mnt_flags: MountFlags::empty(),
        mount_ro: false,
        super_ro: true,
    };
    let mut buf = alloc::string::String::new();

    super::show_mounts(&entry, &mut buf).unwrap();

    assert_eq!(buf, "rootfs / ext4 ro 0 0\n");
}

#[def_test]
fn test_mountinfo_separates_mount_and_super_readonly() {
    let entry = ProcMountEntry {
        mount_id: 1,
        parent_id: 0,
        major: 0,
        minor: 1,
        root: "/".into(),
        mount_point: "/".into(),
        fs_type: "ext4".into(),
        source: "rootfs".into(),
        mnt_flags: MountFlags::empty(),
        mount_ro: false,
        super_ro: true,
    };
    let mut buf = alloc::string::String::new();

    super::show_mountinfo(&entry, &mut buf).unwrap();

    assert_eq!(buf, "1 0 0:1 / / rw - ext4 rootfs ro\n");
}

#[def_test]
fn test_mount_source_uses_devname_when_set() {
    // Create a simple filesystem for testing.
    let fs = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        SimpleDir::<DirMapping>::new_maker(fs, Arc::new(DirMapping::new()))
    });

    let mount =
        Mount::new_root_with_flags_and_devname(&fs, MountFlags::empty(), Some("virtio-blk-0"));

    let result = super::mount_source(&mount);
    assert_eq!(result, Some("virtio-blk-0".to_string()));
}

#[def_test]
fn test_mount_source_falls_back_to_none_when_no_devname() {
    // Create a simple filesystem for testing.
    let fs = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
        SimpleDir::<DirMapping>::new_maker(fs, Arc::new(DirMapping::new()))
    });

    let mount = Mount::new_root_with_flags_and_devname(&fs, MountFlags::empty(), None);

    let result = super::mount_source(&mount);
    assert_eq!(result, None);
}
