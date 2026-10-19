// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn nullfs_is_empty_internal_anchor() {
    let fs = new_superblock();
    let root = fs.root_dir();

    assert_eq!(fs.name(), NULLFS_NAME);
    assert_eq!(root.metadata().inode, NULLFS_ROOT_INO);
    assert_eq!(root.metadata().mode.node_type(), NodeType::Directory);
    assert!(root.lookup("anything").is_err());
    assert_eq!(fs.stat().unwrap().fs_type, NULLFS_MAGIC);
}
