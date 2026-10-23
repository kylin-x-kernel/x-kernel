// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn test_new_procfs_has_expected_name_and_flags() {
    let fs = new_procfs(SuperBlockFlags::empty());

    assert_eq!(fs.name(), "proc");
    let stat = fs.stat().unwrap();
    assert_eq!(stat.fs_type, 0x9fa0);
}

#[def_test(serial)]
fn tracing_capture_stats_is_readable_and_read_only() {
    use kvfs::{Filename, Mount, NodePermission, OpenFlags, VfsError};

    let fs = new_procfs(SuperBlockFlags::empty());
    let root = Mount::new_root(&fs).root_path();
    let file = Filename::new("/tracing/capture_stats")
        .open_with_flags_at(
            &root,
            &root,
            OpenFlags::READ_WRITE.bits(),
            NodePermission::empty(),
            NodePermission::empty(),
            kcred::initial_cred(),
        )
        .unwrap();
    let mut buffer = [0u8; 256];
    let mut position = 0;
    let count = file.read_from(&mut buffer, &mut position).unwrap();
    let text = core::str::from_utf8(&buffer[..count]).unwrap();
    let mut lines = text.lines();
    for field in [
        "retained_records",
        "overwritten_records",
        "oversized_records",
        "uninitialized_records",
    ] {
        let (name, value) = lines.next().unwrap().split_once(": ").unwrap();
        assert_eq!(name, field);
        assert!(value.parse::<usize>().is_ok());
    }
    assert!(lines.next().is_none());
    assert_eq!(file.write(b"0\n"), Err(VfsError::InvalidInput));
}
