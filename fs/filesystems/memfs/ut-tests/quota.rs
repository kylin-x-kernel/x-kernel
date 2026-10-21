// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn zero_page_limit_allows_charges_without_disabling_inode_limit() {
    let quota = TmpfsAccounting::new(0, 1);
    quota.reserve_inode().unwrap();
    assert_eq!(quota.reserve_inode(), Err(KError::StorageFull));
    assert_eq!(quota.try_charge_page(), Ok(()));
    assert_eq!(quota.try_charge_page(), Ok(()));
    let mut stat = kvfs::libfs::simple_statfs(crate::TMPFS_MAGIC);
    quota.statfs(&mut stat);
    assert_eq!((stat.blocks, stat.blocks_free), (0, 0));
    assert_eq!((stat.file_count, stat.free_file_count), (1, 0));
    assert_eq!(quota.used_pages(), 2);
    quota.release_pages(2);
    quota.free_inode();
}

#[def_test]
fn remount_below_usage_returns_einval_without_changing_limit() {
    let quota = TmpfsAccounting::new(4, 0);
    quota.try_charge_page().unwrap();
    quota.try_charge_page().unwrap();
    assert_eq!(quota.reconfigure(Some(1), None), Err(KError::InvalidInput));
    let mut stat = kvfs::libfs::simple_statfs(crate::TMPFS_MAGIC);
    quota.statfs(&mut stat);
    assert_eq!(stat.blocks, 4);
    assert_eq!(quota.used_pages(), 2);
    assert_eq!(quota.reconfigure(Some(2), None), Ok(()));
    assert_eq!(quota.try_charge_page(), Err(KError::StorageFull));
    quota.release_pages(1);
    quota.release_pages(1);
}

#[def_test]
fn capacity_snapshot_uses_one_limit_and_zero_for_unlimited() {
    let quota = TmpfsAccounting::new(4, 0);
    let capacity = || {
        let mut stat = kvfs::libfs::simple_statfs(crate::TMPFS_MAGIC);
        quota.statfs(&mut stat);
        (stat.blocks, stat.blocks_free)
    };
    quota.try_charge_page().unwrap();
    assert_eq!(capacity(), (4, 3));
    quota.reconfigure(Some(8), None).unwrap();
    assert_eq!(capacity(), (8, 7));
    assert_eq!(quota.reconfigure(Some(0), None), Ok(()));
    assert_eq!(capacity(), (0, 0));
    quota.try_charge_page().unwrap();
    assert_eq!(capacity(), (0, 0));
    assert_eq!(quota.used_pages(), 2);
    assert_eq!(quota.reconfigure(Some(2), None), Err(KError::InvalidInput));
    quota.release_pages(2);
}
