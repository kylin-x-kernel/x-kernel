// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn quota_queries_use_effective_identity_and_group_membership() {
    let mut cred = Cred::root();
    cred.set_resgid(Some(200), Some(100), Some(200)).unwrap();
    cred.set_fsgid(200);
    cred.set_supplementary_groups(alloc::vec![300]);
    cred.set_resuid(Some(1000), Some(1000), Some(1000)).unwrap();
    assert!(!cred.has_sys_admin());
    assert_eq!(
        check_permission(&cred, Q_GETQUOTA, QuotaType::User, 1000),
        Ok(())
    );
    assert_eq!(
        check_permission(&cred, Q_GETQUOTA, QuotaType::Group, 100),
        Ok(())
    );
    assert_eq!(
        check_permission(&cred, Q_GETQUOTA, QuotaType::Group, 300),
        Ok(())
    );
    assert_eq!(
        check_permission(&cred, Q_GETQUOTA, QuotaType::Group, 200),
        Err(LinuxError::EPERM.into())
    );
    assert_eq!(
        check_permission(&cred, Q_GETNEXTQUOTA, QuotaType::User, 1000),
        Err(LinuxError::EPERM.into())
    );
    assert_eq!(
        check_permission(&cred, Q_SETQUOTA, QuotaType::User, 1000),
        Err(LinuxError::EPERM.into())
    );
    assert_eq!(
        check_permission(&cred, Q_GETINFO, QuotaType::User, 0),
        Ok(())
    );
}

#[def_test]
fn quota_abi_reports_reservations_and_rounds_limit_units() {
    let value = to_abi(Dqblk {
        used_bytes: 1024,
        reserved_bytes: 512,
        space_hard_limit_bytes: 1025,
        ..Dqblk::default()
    })
    .unwrap();
    assert_eq!(value.current_space, 1536);
    assert_eq!(value.block_hard_limit, 2);
    assert_eq!(value.valid, QIF_ALL);
    assert_eq!(value.abi_padding, 0);
}

#[def_test]
fn quota_update_validates_only_selected_fields_and_ignores_unknown_valid_bits() {
    let value = IfDqblk {
        block_hard_limit: u64::MAX,
        valid: 8 | (1 << 31),
        current_inodes: 5,
        ..IfDqblk::default()
    };
    let (record, fields) = from_abi(value).unwrap();
    assert_eq!(fields, QuotaFields::INODES);
    assert_eq!(record.used_inodes, 5);
    assert_eq!(
        from_abi(IfDqblk { valid: 1, ..value }),
        Err(LinuxError::ERANGE.into())
    );
    assert_eq!(quota_type(3), Err(LinuxError::EINVAL.into()));
}
