// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn exclusive_without_create_does_not_set_create_intent() {
    let flags = OpenHow::from_legacy(O_EXCL, NodePermission::empty())
        .into_open_params()
        .unwrap();

    assert!(!flags.will_create());
    assert!(!flags.is_exclusive_create());
}

#[def_test]
fn exclusive_create_sets_intent_and_disables_final_symlink_following() {
    let flags = OpenHow::from_legacy(O_CREAT | O_EXCL, NodePermission::empty())
        .into_open_params()
        .unwrap();

    assert!(flags.will_create());
    assert!(flags.is_exclusive_create());
    assert!(!flags.lookup_flags().follows_final());
    assert!(flags.flags.contains(OpenFlags::NO_FOLLOW));
}
