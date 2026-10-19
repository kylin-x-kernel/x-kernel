// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use kvfs::{Filename, Mount, NodePermission, Path, SuperBlockFlags};
use unittest::def_test;

#[def_test]
fn bootstrap_snapshot_is_readable_from_shared_devtmpfs() {
    let Some(expected) = khal::firmware::dtb_bytes() else {
        return unittest::TestResult::Ignored;
    };
    let super_block = crate::new_devfs(SuperBlockFlags::empty());
    let second_mount = crate::new_devfs(SuperBlockFlags::empty());
    assert!(alloc::sync::Arc::ptr_eq(&super_block, &second_mount));
    let root = Path::new(Mount::new_root(&super_block), super_block.root_dir());
    let file = Filename::new("firmware-dtb")
        .open_with_flags_at(
            &root,
            &root,
            0,
            NodePermission::default(),
            NodePermission::default(),
            kcred::initial_cred(),
        )
        .expect("bootstrap devtmpfs must publish firmware snapshot");
    let mut bytes = vec![0; expected.len()];
    let mut offset = 0;
    while offset < expected.len() {
        let count = file.read(&mut bytes[offset..]).expect("snapshot read");
        assert!(count > 0, "snapshot must not end before firmware blob");
        offset += count;
    }
    assert_eq!(bytes.as_slice(), expected);
    assert_eq!(file.read(&mut [0; 1]).expect("snapshot EOF"), 0);
}
