// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kvfs::{MountFlags, StatFsFlags, SuperBlockFlags};
use unittest::{assert, assert_eq, def_test};

#[def_test]
fn test_statfs_flags_include_per_mount_flags() {
    let flags = MountFlags::RDONLY
        | MountFlags::NOSUID
        | MountFlags::NODEV
        | MountFlags::NOEXEC
        | MountFlags::NOATIME
        | MountFlags::NODIRATIME
        | MountFlags::RELATIME
        | MountFlags::NOSYMFOLLOW;
    let result = super::statfs_flags(flags, SuperBlockFlags::empty());

    assert!(result.contains(StatFsFlags::RDONLY));
    assert!(result.contains(StatFsFlags::NOSUID));
    assert!(result.contains(StatFsFlags::NODEV));
    assert!(result.contains(StatFsFlags::NOEXEC));
    assert!(result.contains(StatFsFlags::NOATIME));
    assert!(result.contains(StatFsFlags::NODIRATIME));
    assert!(result.contains(StatFsFlags::RELATIME));
    assert!(result.contains(StatFsFlags::NOSYMFOLLOW));
    assert!(result.contains(StatFsFlags::VALID));
}

#[def_test]
fn test_statfs_flags_preserve_supported_superblock_flags() {
    assert_eq!(
        super::statfs_flags(MountFlags::NODEV, SuperBlockFlags::RDONLY),
        StatFsFlags::VALID | StatFsFlags::RDONLY | StatFsFlags::NODEV
    );
}

#[def_test]
fn test_superblock_statfs_flags_filter_mount_only_flags() {
    assert_eq!(
        super::superblock_statfs_flags(
            SuperBlockFlags::RDONLY | SuperBlockFlags::NOATIME | SuperBlockFlags::NODIRATIME
        ),
        StatFsFlags::RDONLY
    );
}
