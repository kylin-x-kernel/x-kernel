// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;

use kvfs::{Filename, Mount, NodePermission, Path, SuperBlockFlags};
use unittest::def_test;

#[def_test]
fn snapshot_failure_leaves_storage_retryable() {
    use super::*;

    let snapshot = LazyInit::new();
    let bytes = [1, 2, 3, 4];
    assert_eq!(
        capture_snapshot_with(&snapshot, Some(&bytes), |_| Err(kvfs::VfsError::NoMemory)),
        Err(kvfs::VfsError::NoMemory)
    );
    assert!(snapshot.get().is_none());
    capture_snapshot_with(&snapshot, Some(&bytes), |source| {
        KBox::from_slice_with_flags(source, AllocFlags::NOWAIT)
    })
    .unwrap();
    assert_eq!(&**snapshot.get().unwrap(), &bytes);
    capture_snapshot_with(&snapshot, Some(&[9]), |_| Err(kvfs::VfsError::NoMemory))
        .unwrap();
    assert_eq!(&**snapshot.get().unwrap(), &bytes);
}

#[def_test]
fn absent_firmware_does_not_prepare_snapshot() {
    let snapshot = lazyinit::LazyInit::new();
    super::capture_snapshot_with(&snapshot, None, |_| Err(kvfs::VfsError::NoMemory))
        .unwrap();
    assert!(snapshot.get().is_none());
}

#[def_test]
fn bootstrap_snapshot_is_readable_from_shared_devtmpfs() {
    let Some(expected) = khal::firmware::dtb_bytes() else {
        return unittest::TestResult::Ignored;
    };
    let super_block = crate::new_devfs(SuperBlockFlags::empty()).unwrap();
    let second_mount = crate::new_devfs(SuperBlockFlags::empty()).unwrap();
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
