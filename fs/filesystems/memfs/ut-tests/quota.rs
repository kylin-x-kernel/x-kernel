// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn remount_below_usage_returns_einval_without_changing_limit() {
    let quota = PageQuota::new(Some(4));
    quota.try_charge_page().unwrap();
    quota.try_charge_page().unwrap();
    assert_eq!(quota.try_set_max_pages(Some(1)), Err(KError::InvalidInput));
    assert_eq!(quota.max_pages(), Some(4));
    assert_eq!(quota.used_pages(), 2);
    assert_eq!(quota.try_set_max_pages(Some(2)), Ok(()));
    assert_eq!(quota.try_charge_page(), Err(KError::StorageFull));
    quota.release_pages(1);
    quota.release_pages(1);
}

#[def_test]
fn capacity_snapshot_uses_one_limit_and_zero_for_unlimited() {
    let quota = PageQuota::new(Some(4));
    quota.try_charge_page().unwrap();
    assert_eq!(quota.statfs_pages(), (4, 3));
    quota.try_set_max_pages(Some(8)).unwrap();
    assert_eq!(quota.statfs_pages(), (8, 7));
    quota.try_set_max_pages(None).unwrap();
    assert_eq!(quota.statfs_pages(), (0, 0));
    quota.release_pages(1);
}
