// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::{KError, LinuxError};
use unittest::{assert, assert_eq, def_test};

use super::fixtures::{MemoryFiles, ids};
use crate::{
    ChargeFlags, Dqblk, QuotaFields, QuotaFormat, QuotaId, QuotaInfo, QuotaState, QuotaType,
};

const FORMAT: QuotaFormat = QuotaFormat::VfsV1;

#[def_test]
fn quota_classes_keep_the_same_numeric_id_independent() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = ids(1000);
    quota
        .alloc_space(&mut io, &owners[..1], 4096, 1, ChargeFlags::WARN)
        .unwrap();
    assert_eq!(quota.get(&mut io, owners[0]).unwrap().used_bytes, 4096);
    assert_eq!(quota.get(&mut io, owners[1]).unwrap().used_bytes, 0);
    assert_eq!(quota.get(&mut io, owners[2]).unwrap().used_bytes, 0);
    assert_eq!(
        QuotaId::new(QuotaType::User, u32::MAX),
        Err(KError::from(LinuxError::EINVAL))
    );
}

#[def_test]
fn space_hard_limit_allows_equality_and_rejects_the_next_byte() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(7)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_hard_limit_bytes: 4096,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_HARD,
            1,
        )
        .unwrap();
    quota
        .alloc_space(&mut io, &[owner], 4096, 1, ChargeFlags::WARN)
        .unwrap();
    assert_eq!(
        quota.alloc_space(&mut io, &[owner], 1, 1, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EDQUOT))
    );
    assert_eq!(quota.get(&mut io, owner).unwrap().used_bytes, 4096);
}

#[def_test]
fn space_grace_expires_at_the_deadline_and_resets_at_the_soft_limit() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(8)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_soft_limit_bytes: 10,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_SOFT,
            100,
        )
        .unwrap();
    quota
        .alloc_space(&mut io, &[owner], 11, 100, ChargeFlags::WARN)
        .unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().space_expires_at, 160);
    quota
        .alloc_space(&mut io, &[owner], 1, 159, ChargeFlags::WARN)
        .unwrap();
    assert_eq!(
        quota.alloc_space(&mut io, &[owner], 1, 160, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EDQUOT))
    );
    quota.free_space(&mut io, &[owner], 2, false).unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().space_expires_at, 0);
    quota
        .alloc_space(&mut io, &[owner], 1, 200, ChargeFlags::WARN)
        .unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().space_expires_at, 260);
}

#[def_test]
fn preallocation_cannot_start_a_soft_limit_grace_period() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(9)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_soft_limit_bytes: 10,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_SOFT,
            1,
        )
        .unwrap();
    assert_eq!(
        quota.alloc_space(&mut io, &[owner], 11, 100, ChargeFlags::empty()),
        Err(KError::from(LinuxError::EDQUOT))
    );
    let record = quota.get(&mut io, owner).unwrap();
    assert_eq!(record.used_bytes, 0);
    assert_eq!(record.space_expires_at, 0);
    quota
        .alloc_space(&mut io, &[owner], 11, 100, ChargeFlags::NOFAIL)
        .unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().space_expires_at, 0);
}

#[def_test]
fn rejected_group_charge_does_not_start_the_user_grace_period() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = ids(10);
    quota
        .set(
            &mut io,
            owners[0],
            Dqblk {
                space_soft_limit_bytes: 10,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_SOFT,
            1,
        )
        .unwrap();
    quota
        .set(
            &mut io,
            owners[1],
            Dqblk {
                space_hard_limit_bytes: 5,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_HARD,
            1,
        )
        .unwrap();
    assert_eq!(
        quota.alloc_space(&mut io, &owners, 11, 100, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EDQUOT))
    );
    for owner in owners {
        let record = quota.get(&mut io, owner).unwrap();
        assert_eq!(record.used_bytes, 0);
        assert_eq!(record.space_expires_at, 0);
    }
}

#[def_test]
fn project_inode_limit_rejects_the_whole_inode_charge() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = ids(11);
    quota
        .set(
            &mut io,
            owners[2],
            Dqblk {
                inode_hard_limit: 1,
                ..Dqblk::default()
            },
            QuotaFields::INODE_HARD,
            1,
        )
        .unwrap();
    quota.alloc_inode(&mut io, &owners, 1, false).unwrap();
    assert_eq!(
        quota.alloc_inode(&mut io, &owners, 2, false),
        Err(KError::from(LinuxError::EDQUOT))
    );
    for owner in owners {
        assert_eq!(quota.get(&mut io, owner).unwrap().used_inodes, 1);
    }
}

#[def_test]
fn inode_soft_limit_uses_its_own_grace_period() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(12)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                inode_soft_limit: 1,
                ..Dqblk::default()
            },
            QuotaFields::INODE_SOFT,
            1,
        )
        .unwrap();
    quota.alloc_inode(&mut io, &[owner], 100, false).unwrap();
    quota.alloc_inode(&mut io, &[owner], 100, false).unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().inode_expires_at, 130);
    assert_eq!(
        quota.alloc_inode(&mut io, &[owner], 130, false),
        Err(KError::from(LinuxError::EDQUOT))
    );
    quota.free_inode(&mut io, &[owner]).unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().inode_expires_at, 0);
}

#[def_test]
fn zero_grace_refuses_further_growth_at_the_same_real_time() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(13)[0];
    quota
        .set_grace_times(&mut io, QuotaType::User, 0, 0)
        .unwrap();
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_soft_limit_bytes: 1,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_SOFT,
            1,
        )
        .unwrap();
    quota
        .alloc_space(&mut io, &[owner], 2, 100, ChargeFlags::WARN)
        .unwrap();
    assert_eq!(
        quota.alloc_space(&mut io, &[owner], 1, 100, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EDQUOT))
    );
}

#[def_test]
fn reservations_count_against_limits_and_claim_does_not_charge_twice() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = ids(14);
    for owner in owners {
        quota
            .set(
                &mut io,
                owner,
                Dqblk {
                    space_hard_limit_bytes: 4096,
                    ..Dqblk::default()
                },
                QuotaFields::SPACE_HARD,
                1,
            )
            .unwrap();
    }
    quota
        .alloc_space(
            &mut io,
            &owners,
            4096,
            1,
            ChargeFlags::WARN | ChargeFlags::RESERVE,
        )
        .unwrap();
    assert_eq!(
        quota.alloc_space(&mut io, &owners, 1, 1, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EDQUOT))
    );
    quota.claim_space(&mut io, &owners, 1024).unwrap();
    for owner in owners {
        let data = quota.get(&mut io, owner).unwrap();
        assert_eq!(data.used_bytes, 1024);
        assert_eq!(data.reserved_bytes, 3072);
        assert_eq!(data.total_bytes().unwrap(), 4096);
    }
    quota.reclaim_space(&mut io, &owners, 1024).unwrap();
    assert_eq!(quota.get(&mut io, owners[0]).unwrap().reserved_bytes, 4096);
}

#[def_test]
fn claiming_admitted_reservations_after_grace_expiry_still_succeeds() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(15)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_soft_limit_bytes: 1,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_SOFT,
            1,
        )
        .unwrap();
    quota
        .alloc_space(
            &mut io,
            &[owner],
            2,
            100,
            ChargeFlags::WARN | ChargeFlags::RESERVE,
        )
        .unwrap();
    quota.claim_space(&mut io, &[owner], 2).unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().used_bytes, 2);
    assert_eq!(
        quota.alloc_space(&mut io, &[owner], 1, 160, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EDQUOT))
    );
}

#[def_test]
fn a_bad_claim_is_rejected_before_any_owner_changes() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = ids(16);
    quota
        .alloc_space(
            &mut io,
            &owners[..1],
            100,
            1,
            ChargeFlags::WARN | ChargeFlags::RESERVE,
        )
        .unwrap();
    assert_eq!(
        quota.claim_space(&mut io, &owners, 100),
        Err(KError::from(LinuxError::EINVAL))
    );
    assert_eq!(quota.get(&mut io, owners[0]).unwrap().reserved_bytes, 100);
    assert_eq!(quota.get(&mut io, owners[0]).unwrap().used_bytes, 0);
}

#[def_test]
fn quota_files_never_persist_delayed_reservations() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(17)[0];
    quota
        .alloc_space(&mut io, &[owner], 4096, 1, ChargeFlags::WARN)
        .unwrap();
    quota
        .alloc_space(
            &mut io,
            &[owner],
            8192,
            1,
            ChargeFlags::WARN | ChargeFlags::RESERVE,
        )
        .unwrap();
    quota.sync(&mut io, QuotaType::User).unwrap();
    let mut reopened = io.enable_all(FORMAT);
    let data = reopened.get(&mut io, owner).unwrap();
    assert_eq!(data.used_bytes, 4096);
    assert_eq!(data.reserved_bytes, 0);
    assert_eq!(
        quota.disable(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EBUSY))
    );
    assert_eq!(
        quota.suspend(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EBUSY))
    );
    assert_eq!(
        quota.release(&mut io, owner),
        Err(KError::from(LinuxError::EBUSY))
    );
}

#[def_test]
fn resource_bypass_and_nofail_preserve_usage_accounting() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(18)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_hard_limit_bytes: 1,
                inode_hard_limit: 1,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_HARD | QuotaFields::INODE_HARD,
            1,
        )
        .unwrap();
    quota
        .alloc_space(
            &mut io,
            &[owner],
            2,
            1,
            ChargeFlags::WARN | ChargeFlags::BYPASS_LIMITS,
        )
        .unwrap();
    quota
        .alloc_space(
            &mut io,
            &[owner],
            3,
            1,
            ChargeFlags::WARN | ChargeFlags::NOFAIL,
        )
        .unwrap();
    quota.alloc_inode(&mut io, &[owner], 1, true).unwrap();
    quota.alloc_inode(&mut io, &[owner], 1, true).unwrap();
    let data = quota.get(&mut io, owner).unwrap();
    assert_eq!(data.used_bytes, 5);
    assert_eq!(data.used_inodes, 2);
}

#[def_test]
fn accounting_only_mode_can_enable_limits_without_losing_usage() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(19)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_hard_limit_bytes: 1,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_HARD,
            1,
        )
        .unwrap();
    quota.set_limits_enabled(QuotaType::User, false).unwrap();
    quota
        .alloc_space(&mut io, &[owner], 10, 1, ChargeFlags::WARN)
        .unwrap();
    quota.set_limits_enabled(QuotaType::User, true).unwrap();
    assert_eq!(
        quota.alloc_space(&mut io, &[owner], 1, 1, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EDQUOT))
    );
    assert_eq!(quota.get(&mut io, owner).unwrap().used_bytes, 10);
}

#[def_test]
fn administrative_updates_honor_the_mask_and_preserve_reservations() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(20)[0];
    quota
        .alloc_space(
            &mut io,
            &[owner],
            100,
            1,
            ChargeFlags::WARN | ChargeFlags::RESERVE,
        )
        .unwrap();
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                used_bytes: 50,
                reserved_bytes: 999,
                inode_hard_limit: -1,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_USED,
            1,
        )
        .unwrap();
    let data = quota.get(&mut io, owner).unwrap();
    assert_eq!(data.used_bytes, -50);
    assert_eq!(data.reserved_bytes, 100);
    assert_eq!(data.total_bytes().unwrap(), 50);
    assert_eq!(data.inode_hard_limit, 0);
}

#[def_test]
fn supplied_timers_are_cleared_below_soft_limits() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(21)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_soft_limit_bytes: 10,
                used_bytes: 11,
                space_expires_at: 500,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_SOFT | QuotaFields::SPACE_USED | QuotaFields::SPACE_TIME,
            100,
        )
        .unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().space_expires_at, 500);
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                used_bytes: 10,
                space_expires_at: 900,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_USED | QuotaFields::SPACE_TIME,
            200,
        )
        .unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().space_expires_at, 0);
}

#[def_test]
fn grace_duration_updates_do_not_extend_existing_deadlines() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(22)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_soft_limit_bytes: 1,
                used_bytes: 2,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_SOFT | QuotaFields::SPACE_USED,
            100,
        )
        .unwrap();
    quota
        .set_grace_times(&mut io, QuotaType::User, 3600, 7200)
        .unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().space_expires_at, 160);
    quota.sync(&mut io, QuotaType::User).unwrap();
    let reopened = io.enable_all(FORMAT);
    assert_eq!(
        reopened
            .info(QuotaType::User)
            .unwrap()
            .space_grace_seconds(),
        3600
    );
    assert_eq!(
        reopened
            .info(QuotaType::User)
            .unwrap()
            .inode_grace_seconds(),
        7200
    );
}

#[def_test]
fn overflow_and_duplicate_quota_classes_do_not_modify_usage() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = ids(23);
    quota
        .set(
            &mut io,
            owners[1],
            Dqblk {
                used_bytes: i64::MAX,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_USED,
            1,
        )
        .unwrap();
    assert_eq!(
        quota.alloc_space(&mut io, &owners, 1, 1, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EOVERFLOW))
    );
    assert_eq!(quota.get(&mut io, owners[0]).unwrap().used_bytes, 0);
    let writes = io.writes.get();
    assert_eq!(
        quota.alloc_space(&mut io, &[owners[0], ids(24)[0]], 1, 1, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EINVAL))
    );
    assert_eq!(io.writes.get(), writes);
}

#[def_test]
fn dirty_record_write_failure_is_retried_without_losing_the_update() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(1)[0];
    quota
        .alloc_space(&mut io, &[owner], 4096, 1, ChargeFlags::WARN)
        .unwrap();
    io.fail_write_at.set(Some(5 * 1024 + 16));
    assert_eq!(
        quota.sync(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(quota.get(&mut io, owner).unwrap().used_bytes, 4096);
    assert_eq!(
        quota.disable(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EIO))
    );
    assert!(quota.info(QuotaType::User).is_some());
    io.fail_write_at.set(None);
    quota.disable(&mut io, QuotaType::User).unwrap();
    quota
        .enable(
            &mut io,
            QuotaType::User,
            &alloc::sync::Arc::new(QuotaType::User),
            FORMAT,
            QuotaState::USAGE_ENABLED,
        )
        .unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().used_bytes, 4096);
}

#[def_test]
fn dirty_file_information_is_not_lost_on_a_short_write() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    io.has_short_writes.set(true);
    assert_eq!(
        quota.set_grace_times(&mut io, QuotaType::User, 1000, 2000),
        Err(KError::from(LinuxError::EIO))
    );
    io.has_short_writes.set(false);
    quota.sync(&mut io, QuotaType::User).unwrap();
    let reopened = io.enable_all(FORMAT);
    assert_eq!(
        reopened
            .info(QuotaType::User)
            .unwrap()
            .space_grace_seconds(),
        1000
    );
}

#[def_test]
fn suspension_preserves_usage_but_skips_new_charges_until_resume() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(25)[0];
    quota
        .alloc_space(&mut io, &[owner], 100, 1, ChargeFlags::WARN)
        .unwrap();
    quota.suspend(&mut io, QuotaType::User).unwrap();
    assert_eq!(
        quota.get(&mut io, owner),
        Err(KError::from(LinuxError::ESRCH))
    );
    quota
        .alloc_space(&mut io, &[owner], 200, 1, ChargeFlags::WARN)
        .unwrap();
    quota.resume(QuotaType::User).unwrap();
    assert_eq!(quota.get(&mut io, owner).unwrap().used_bytes, 100);
    quota.disable(&mut io, QuotaType::User).unwrap();
    assert!(quota.info(QuotaType::User).is_none());
}

#[def_test]
fn freeing_more_than_the_usage_stops_at_zero() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = ids(26);
    quota
        .alloc_space(&mut io, &owners, 1, 1, ChargeFlags::WARN)
        .unwrap();
    quota.free_space(&mut io, &owners, 100, false).unwrap();
    quota.free_space(&mut io, &owners, 100, true).unwrap();
    quota.free_inode(&mut io, &owners).unwrap();
    for owner in owners {
        let data = quota.get(&mut io, owner).unwrap();
        assert_eq!(data.used_bytes, 0);
        assert_eq!(data.reserved_bytes, 0);
        assert_eq!(data.used_inodes, 0);
    }
}

#[def_test]
fn enable_rejects_invalid_state_without_publishing_partial_state() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = QuotaInfo::new();
    assert_eq!(
        quota.enable(
            &mut io,
            QuotaType::User,
            &alloc::sync::Arc::new(QuotaType::User),
            FORMAT,
            QuotaState::LIMITS_ENABLED
        ),
        Err(KError::from(LinuxError::EINVAL))
    );
    assert!(quota.info(QuotaType::User).is_none());
    io.has_short_reads.set(true);
    assert_eq!(
        quota.enable(
            &mut io,
            QuotaType::User,
            &alloc::sync::Arc::new(QuotaType::User),
            FORMAT,
            QuotaState::USAGE_ENABLED
        ),
        Err(KError::from(LinuxError::EIO))
    );
    assert!(quota.info(QuotaType::User).is_none());
}

#[def_test]
fn releasing_one_record_does_not_write_back_unrelated_dirty_records() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let first = ids(1)[0];
    let second = ids(2)[0];
    quota.alloc_inode(&mut io, &[first], 1, false).unwrap();
    quota.alloc_inode(&mut io, &[second], 1, false).unwrap();
    io.fail_write_at.set(Some(5 * 1024 + 16 + 72));
    quota.release(&mut io, first).unwrap();
    let mut reopened = io.enable_all(FORMAT);
    assert_eq!(reopened.get(&mut io, first).unwrap().used_inodes, 1);
    assert_eq!(reopened.get(&mut io, second).unwrap().used_inodes, 0);
    assert_eq!(quota.get(&mut io, second).unwrap().used_inodes, 1);
}

#[def_test]
fn reclaim_rejects_insufficient_allocation_for_every_class_before_mutation() {
    for format in [QuotaFormat::VfsV0, QuotaFormat::VfsV1] {
        let mut io = MemoryFiles::new(format);
        let mut quota = io.enable_all(format);
        let owners = ids(41);
        quota
            .alloc_space(&mut io, &owners[..2], 200, 1, ChargeFlags::WARN)
            .unwrap();
        quota
            .alloc_space(&mut io, &owners[2..], 100, 1, ChargeFlags::WARN)
            .unwrap();
        let before = owners.map(|id| quota.get(&mut io, id).unwrap());
        assert_eq!(
            quota.reclaim_space(&mut io, &owners, 200),
            Err(KError::from(LinuxError::EINVAL))
        );
        for (owner, expected) in owners.into_iter().zip(before) {
            assert_eq!(quota.get(&mut io, owner).unwrap(), expected);
        }
        quota.free_space(&mut io, &owners, 200, true).unwrap();
        for kind in QuotaType::ALL {
            quota.sync(&mut io, kind).unwrap();
        }
        drop(quota);
        let mut reopened = io.enable_all(format);
        for (owner, expected) in owners.into_iter().zip(before) {
            assert_eq!(reopened.get(&mut io, owner).unwrap(), expected);
        }
        reopened.reclaim_space(&mut io, &owners, 100).unwrap();
        reopened.claim_space(&mut io, &owners, 100).unwrap();
        for (owner, expected) in owners.into_iter().zip(before) {
            assert_eq!(reopened.get(&mut io, owner).unwrap(), expected);
        }
    }
}

#[def_test]
fn administrative_negative_usage_cannot_be_reclaimed_as_allocated_space() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(42)[0];
    quota
        .alloc_space(
            &mut io,
            &[owner],
            100,
            1,
            ChargeFlags::WARN | ChargeFlags::RESERVE,
        )
        .unwrap();
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                used_bytes: 50,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_USED,
            1,
        )
        .unwrap();
    let before = quota.get(&mut io, owner).unwrap();
    assert_eq!(before.used_bytes, -50);
    assert_eq!(
        quota.reclaim_space(&mut io, &[owner], 1),
        Err(KError::from(LinuxError::EINVAL))
    );
    assert_eq!(quota.get(&mut io, owner).unwrap(), before);
}

#[def_test]
fn sync_continues_past_a_failed_record_and_retries_only_pending_data() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = [ids(1)[0], ids(2)[0], ids(3)[0]];
    for (index, owner) in owners.into_iter().enumerate() {
        quota
            .alloc_space(
                &mut io,
                &[owner],
                (index as i64 + 1) * 100,
                1,
                ChargeFlags::WARN,
            )
            .unwrap();
    }
    io.fail_write_at.set(Some(8));
    assert_eq!(
        quota.set_grace_times(&mut io, QuotaType::User, 180, 240),
        Err(KError::from(LinuxError::EIO))
    );
    io.fail_write_at.set(Some(5 * 1024 + 16));
    let writes = io.writes.get();
    assert_eq!(
        quota.sync(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(io.writes.get() - writes, 4);
    let mut reopened = io.enable_all(FORMAT);
    assert_eq!(
        reopened
            .info(QuotaType::User)
            .unwrap()
            .space_grace_seconds(),
        180
    );
    assert_eq!(reopened.get(&mut io, owners[0]).unwrap().used_bytes, 0);
    assert_eq!(reopened.get(&mut io, owners[1]).unwrap().used_bytes, 200);
    assert_eq!(reopened.get(&mut io, owners[2]).unwrap().used_bytes, 300);
    drop(reopened);
    let writes = io.writes.get();
    assert_eq!(
        quota.sync(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(io.writes.get() - writes, 1);
    io.fail_write_at.set(None);
    quota.sync(&mut io, QuotaType::User).unwrap();
    drop(quota);
    let mut reopened = io.enable_all(FORMAT);
    assert_eq!(reopened.get(&mut io, owners[0]).unwrap().used_bytes, 100);
}

#[def_test]
fn an_unavailable_quota_class_takes_precedence_over_an_unknown_field_mask() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = QuotaInfo::new();
    let owner = ids(43)[0];
    let unknown = QuotaFields::from_bits_retain(1 << 15);
    assert_eq!(
        quota.set(&mut io, owner, Dqblk::default(), unknown, 1),
        Err(KError::from(LinuxError::ESRCH))
    );
    quota
        .enable(
            &mut io,
            QuotaType::User,
            &alloc::sync::Arc::new(QuotaType::User),
            FORMAT,
            QuotaState::USAGE_ENABLED,
        )
        .unwrap();
    assert_eq!(
        quota.set(&mut io, owner, Dqblk::default(), unknown, 1),
        Err(KError::from(LinuxError::EINVAL))
    );
    quota.suspend(&mut io, QuotaType::User).unwrap();
    assert_eq!(
        quota.set(&mut io, owner, Dqblk::default(), unknown, 1),
        Err(KError::from(LinuxError::ESRCH))
    );
    assert_eq!(io.writes.get(), 0);
}

#[def_test]
fn setting_grace_times_writes_file_info_without_flushing_unrelated_records() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(44)[0];
    quota.alloc_inode(&mut io, &[owner], 1, false).unwrap();
    quota
        .set_grace_times(&mut io, QuotaType::User, 3600, 7200)
        .unwrap();
    let mut reopened = io.enable_all(FORMAT);
    assert_eq!(
        reopened
            .info(QuotaType::User)
            .unwrap()
            .space_grace_seconds(),
        3600
    );
    assert_eq!(
        reopened
            .info(QuotaType::User)
            .unwrap()
            .inode_grace_seconds(),
        7200
    );
    assert_eq!(reopened.get(&mut io, owner).unwrap().used_inodes, 0);
    assert_eq!(quota.get(&mut io, owner).unwrap().used_inodes, 1);
}

#[def_test]
fn rejected_admission_keeps_usage_unchanged_after_missing_record_acquisition() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = ids(45);
    quota
        .set(
            &mut io,
            owners[1],
            Dqblk {
                space_hard_limit_bytes: 1,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_HARD,
            1,
        )
        .unwrap();
    assert_eq!(quota.next_id(&mut io, QuotaType::User, 0).unwrap(), None);
    let writes = io.writes.get();
    assert_eq!(
        quota.alloc_space(&mut io, &owners, 2, 1, ChargeFlags::WARN),
        Err(KError::from(LinuxError::EDQUOT))
    );
    assert!(io.writes.get() > writes);
    assert_eq!(
        quota.next_id(&mut io, QuotaType::User, 0).unwrap(),
        Some(owners[0])
    );
    for owner in owners {
        assert_eq!(quota.get(&mut io, owner).unwrap().used_bytes, 0);
    }
    quota.release(&mut io, owners[0]).unwrap();
    assert_eq!(quota.next_id(&mut io, QuotaType::User, 0).unwrap(), None);
}

#[def_test]
fn sync_preserves_the_first_error_and_continues_after_different_failures() {
    let format = QuotaFormat::VfsV0;
    let mut io = MemoryFiles::new(format);
    let mut quota = io.enable_all(format);
    let owners = [ids(1)[0], ids(2)[0], ids(3)[0]];
    for owner in owners {
        quota.alloc_inode(&mut io, &[owner], 1, false).unwrap();
    }
    quota
        .set(
            &mut io,
            owners[1],
            Dqblk {
                used_inodes: i64::from(u32::MAX) + 1,
                ..Dqblk::default()
            },
            QuotaFields::INODES,
            1,
        )
        .unwrap();
    io.fail_write_at.set(Some(5 * 1024 + 16));
    assert_eq!(
        quota.sync(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EIO))
    );
    let mut reopened = io.enable_all(format);
    assert_eq!(reopened.get(&mut io, owners[2]).unwrap().used_inodes, 1);
    drop(reopened);
    io.fail_write_at.set(None);
    assert_eq!(
        quota.sync(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EOVERFLOW))
    );
    quota
        .set(
            &mut io,
            owners[1],
            Dqblk {
                used_inodes: 1,
                ..Dqblk::default()
            },
            QuotaFields::INODES,
            1,
        )
        .unwrap();
    quota.sync(&mut io, QuotaType::User).unwrap();
    drop(quota);
    let mut reopened = io.enable_all(format);
    for owner in owners {
        assert_eq!(reopened.get(&mut io, owner).unwrap().used_inodes, 1);
    }
}

#[def_test]
fn backing_file_is_retained_through_suspend_and_failed_disable() {
    use alloc::sync::Arc;

    let mut io = MemoryFiles::new(FORMAT);
    let file = Arc::new(QuotaType::User);
    let mut quota = QuotaInfo::new();
    quota
        .enable(
            &mut io,
            QuotaType::User,
            &file,
            FORMAT,
            QuotaState::USAGE_ENABLED,
        )
        .unwrap();
    assert!(Arc::ptr_eq(
        quota.info(QuotaType::User).unwrap().file(),
        &file
    ));
    assert_eq!(Arc::strong_count(&file), 2);
    quota.suspend(&mut io, QuotaType::User).unwrap();
    assert_eq!(Arc::strong_count(&file), 2);
    quota.resume(QuotaType::User).unwrap();
    quota.alloc_inode(&mut io, &[ids(9)[0]], 0, false).unwrap();
    io.has_short_writes.set(true);
    assert_eq!(
        quota.disable(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(Arc::strong_count(&file), 2);
    io.has_short_writes.set(false);
    let released = quota.disable(&mut io, QuotaType::User).unwrap();
    assert!(quota.info(QuotaType::User).is_none());
    assert_eq!(Arc::strong_count(&file), 2);
    drop(released);
    assert_eq!(Arc::strong_count(&file), 1);
    assert_eq!(quota.next_dirty_id(None), None);
}

#[def_test]
fn failed_enable_does_not_retain_the_supplied_file() {
    use alloc::sync::Arc;

    let mut io = MemoryFiles::new(FORMAT);
    let file = Arc::new(QuotaType::Group);
    let mut quota = QuotaInfo::new();
    assert_eq!(
        quota.enable(
            &mut io,
            QuotaType::User,
            &file,
            FORMAT,
            QuotaState::USAGE_ENABLED
        ),
        Err(KError::from(LinuxError::EINVAL))
    );
    assert!(quota.info(QuotaType::User).is_none());
    assert_eq!(Arc::strong_count(&file), 1);
}

#[def_test]
fn record_writeback_cursor_advances_past_failed_records_and_keeps_class_order() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owners = ids(17);
    quota.alloc_inode(&mut io, &owners, 0, false).unwrap();
    assert_eq!(quota.next_dirty_id(None), Some(owners[0]));
    io.has_short_writes.set(true);
    assert_eq!(
        quota.sync_dquot(&mut io, owners[0]),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(quota.next_dirty_id(None), Some(owners[0]));
    assert_eq!(quota.next_dirty_id(Some(owners[0])), Some(owners[1]));
    io.has_short_writes.set(false);
    for owner in owners {
        quota.sync_dquot(&mut io, owner).unwrap();
        quota.sync_info(&mut io, owner.kind()).unwrap();
    }
    assert_eq!(quota.next_dirty_id(None), None);
}

#[def_test]
fn writeback_does_not_finish_a_failed_record_acquisition() {
    let mut io = MemoryFiles::new(FORMAT);
    let mut quota = io.enable_all(FORMAT);
    let owner = ids(23)[0];
    io.has_short_writes.set(true);
    assert_eq!(
        quota.get(&mut io, owner),
        Err(KError::from(LinuxError::EIO))
    );
    io.has_short_writes.set(false);
    let writes = io.writes.get();
    assert_eq!(
        quota.sync_dquot(&mut io, owner),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(
        quota.sync(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(io.writes.get(), writes);
    assert_eq!(quota.next_dirty_id(None), Some(owner));

    quota.get(&mut io, owner).unwrap();
    quota.alloc_inode(&mut io, &[owner], 0, false).unwrap();
    quota.sync_dquot(&mut io, owner).unwrap();
    let mut reopened = io.enable_all(FORMAT);
    assert_eq!(reopened.get(&mut io, owner).unwrap().used_inodes, 1);
}
