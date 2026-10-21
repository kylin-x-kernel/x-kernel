// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{vec, vec::Vec};

use unittest::{assert_eq, def_test};

use super::*;

#[def_test]
fn mutation_reuses_unchanged_shared_external_block_when_disk_is_full() {
    let old_external = vec![Ext4Xattr {
        namespace: Ext4XattrNamespace::Security,
        name: Vec::from(&b"external"[..]),
        value: vec![0x45; 200],
    }];
    let updated_inline = Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"inline"[..]),
        value: Vec::from(&b"updated"[..]),
    };
    let inline = vec![Ext4Xattr {
        namespace: Ext4XattrNamespace::User,
        name: Vec::from(&b"inline"[..]),
        value: Vec::from(&b"old"[..]),
    }];
    let PlannedXattrMutation::Changed(plan) = plan_xattr_set_storage(
        &inline,
        &old_external,
        Ext4XattrNamespace::User,
        b"inline",
        b"updated",
        Ext4XattrSetMode::Replace,
        0,
        256,
        4096,
        XattrStorageLayout::External { shared: true },
        0,
    )
    .unwrap() else {
        panic!("different value must change storage");
    };

    assert_eq!(plan.storage.inline, vec![updated_inline]);
    assert_eq!(plan.storage.external, old_external);
    assert_eq!(plan.external_block_action, ExternalXattrBlockAction::Reuse);
    assert_eq!(
        xattr_mutation_credit_count(
            XattrStorageLayout::External { shared: true },
            plan.storage.layout(),
            plan.external_block_action,
        ),
        XATTR_INODE_UPDATE_CREDITS
    );
}

#[def_test]
fn mutation_marks_new_external_block_for_preallocation() {
    let PlannedXattrMutation::Changed(plan) = plan_xattr_set_storage(
        &[],
        &[],
        Ext4XattrNamespace::User,
        b"key",
        &[0x5a; 88],
        Ext4XattrSetMode::Create,
        16,
        0,
        4096,
        XattrStorageLayout::Inline,
        1,
    )
    .unwrap() else {
        panic!("new value must change storage");
    };

    assert_eq!(plan.extra_isize, 16);
    assert_eq!(
        plan.external_block_action,
        ExternalXattrBlockAction::Allocate
    );
}
