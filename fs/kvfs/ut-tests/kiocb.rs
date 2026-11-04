// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert_eq, def_test};

use super::*;

struct TestFileOperations;

impl crate::FileOperations for TestFileOperations {}

fn test_file(flags: OpenFlags) -> Arc<VfsFile> {
    let root = crate::Mount::new_root(&crate::nullfs::new_superblock()).root_path();
    crate::VfsFileBuilder::from_path_state(
        root,
        crate::FMode::READ,
        flags,
        Arc::new(TestFileOperations),
        kcred::initial_cred(),
    )
    .finish()
    .unwrap()
}

#[def_test]
fn empty_status_flags_yield_empty_control_flags() {
    assert_eq!(
        KiocbFlags::from_open_flags(OpenFlags::empty()),
        KiocbFlags::empty()
    );
}

#[def_test]
fn direct_status_flag_maps_to_direct_control_flag() {
    assert_eq!(
        KiocbFlags::from_open_flags(OpenFlags::DIRECT),
        KiocbFlags::DIRECT
    );
}

#[def_test]
fn dsync_status_flag_maps_to_dsync_control_flag() {
    assert_eq!(
        KiocbFlags::from_open_flags(OpenFlags::DSYNC),
        KiocbFlags::DSYNC
    );
}

#[def_test]
fn sync_status_flag_also_sets_dsync() {
    // Linux encodes O_SYNC as __O_SYNC | O_DSYNC.
    let flags = KiocbFlags::from_open_flags(OpenFlags::SYNC);
    assert!(flags.contains(KiocbFlags::SYNC));
    assert!(flags.contains(KiocbFlags::DSYNC));
    assert!(!flags.contains(KiocbFlags::DIRECT));
}

#[def_test]
fn combined_status_flags_map_one_to_one() {
    let flags = KiocbFlags::from_open_flags(OpenFlags::DIRECT | OpenFlags::DSYNC);
    assert_eq!(flags, KiocbFlags::DIRECT | KiocbFlags::DSYNC);
}

#[def_test]
fn new_records_open_file_flags() {
    let file = test_file(OpenFlags::DIRECT | OpenFlags::DSYNC);
    let iocb = Kiocb::new(&file, 7);
    assert_eq!(iocb.ki_pos(), 7);
    assert_eq!(iocb.flags(), KiocbFlags::DIRECT | KiocbFlags::DSYNC);
}

#[def_test]
fn later_flags_change_does_not_affect_existing_control_block() {
    let file = test_file(OpenFlags::DIRECT);
    let iocb = Kiocb::new(&file, 0);
    file.replace_flags(OpenFlags::DIRECT, OpenFlags::empty());
    assert_eq!(iocb.flags(), KiocbFlags::DIRECT);
    assert!(Kiocb::new(&file, 0).flags().is_empty());
}