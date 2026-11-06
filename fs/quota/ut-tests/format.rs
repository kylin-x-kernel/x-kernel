// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kerrno::{KError, LinuxError};
use unittest::{assert, assert_eq, def_test};

use super::fixtures::{MemoryFiles, ids};
use crate::{
    ChargeFlags, Dqblk, QuotaFields, QuotaFormat, QuotaId, QuotaInfo, QuotaState, QuotaType,
};

#[def_test]
fn both_v2_revisions_roundtrip_usage_limits_and_grace_deadlines() {
    for format in [QuotaFormat::VfsV0, QuotaFormat::VfsV1] {
        let mut io = MemoryFiles::new(format);
        let mut quota = io.enable_all(format);
        for owner in ids(0x1234_5678) {
            let data = Dqblk {
                space_hard_limit_bytes: 8193,
                space_soft_limit_bytes: 4097,
                used_bytes: 6000,
                reserved_bytes: 0,
                inode_hard_limit: 100,
                inode_soft_limit: 50,
                used_inodes: 60,
                space_expires_at: 400,
                inode_expires_at: 500,
            };
            quota
                .set(&mut io, owner, data, QuotaFields::all(), 100)
                .unwrap();
            quota.sync(&mut io, owner.kind()).unwrap();
            quota.release(&mut io, owner).unwrap();
            let reopened = quota.get(&mut io, owner).unwrap();
            assert_eq!(reopened.space_hard_limit_bytes, 9216);
            assert_eq!(reopened.space_soft_limit_bytes, 5120);
            assert_eq!(reopened.used_bytes, 6000);
            assert_eq!(reopened.used_inodes, 60);
            assert_eq!(reopened.inode_hard_limit, 100);
            assert_eq!(reopened.space_expires_at, 400);
            assert_eq!(reopened.inode_expires_at, 500);
        }
    }
}

#[def_test]
fn zero_id_with_all_zero_values_uses_the_nonempty_slot_escape() {
    for format in [QuotaFormat::VfsV0, QuotaFormat::VfsV1] {
        let mut io = MemoryFiles::new(format);
        let mut quota = io.enable_all(format);
        let owner = ids(0)[0];
        assert_eq!(quota.get(&mut io, owner).unwrap(), Dqblk::default());
        let mut reopened = io.enable_all(format);
        assert_eq!(reopened.get(&mut io, owner).unwrap(), Dqblk::default());
        assert_eq!(
            reopened.next_id(&mut io, QuotaType::User, 0).unwrap(),
            Some(owner)
        );
        let files = io.files.borrow();
        let entry_size = if format == QuotaFormat::VfsV0 { 48 } else { 72 };
        assert_eq!(files[0][5 * 1024 + 16 + entry_size - 8], 1);
    }
}

#[def_test]
fn full_partial_and_empty_data_blocks_reuse_their_free_lists() {
    for (format, capacity) in [(QuotaFormat::VfsV0, 21_u32), (QuotaFormat::VfsV1, 14)] {
        let mut io = MemoryFiles::new(format);
        let mut quota = io.enable_all(format);
        for id in 0..capacity * 2 {
            quota.alloc_inode(&mut io, &[ids(id)[0]], 1, false).unwrap();
        }
        quota.sync(&mut io, QuotaType::User).unwrap();
        let file_size = io.files.borrow()[0].len();
        for id in 0..capacity * 2 {
            let owner = ids(id)[0];
            quota.free_inode(&mut io, &[owner]).unwrap();
            quota.release(&mut io, owner).unwrap();
        }
        assert_eq!(quota.next_id(&mut io, QuotaType::User, 0).unwrap(), None);
        for id in 0..capacity * 2 {
            quota.alloc_inode(&mut io, &[ids(id)[0]], 1, false).unwrap();
        }
        quota.sync(&mut io, QuotaType::User).unwrap();
        assert_eq!(io.files.borrow()[0].len(), file_size);
        let mut reopened = io.enable_all(format);
        for id in 0..capacity * 2 {
            assert_eq!(reopened.get(&mut io, ids(id)[0]).unwrap().used_inodes, 1);
        }
    }
}

#[def_test]
fn releasing_one_id_does_not_remove_other_entries_in_the_same_data_block() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    let first = ids(1)[0];
    let second = ids(2)[0];
    quota.get(&mut io, first).unwrap();
    quota.alloc_inode(&mut io, &[second], 1, false).unwrap();
    quota.release(&mut io, first).unwrap();
    quota.sync(&mut io, QuotaType::User).unwrap();
    assert_eq!(
        quota.next_id(&mut io, QuotaType::User, 0).unwrap(),
        Some(second)
    );
    let mut reopened = io.enable_all(QuotaFormat::VfsV1);
    assert_eq!(reopened.get(&mut io, second).unwrap().used_inodes, 1);
}

#[def_test]
fn iteration_is_inclusive_across_radix_boundaries_and_never_wraps() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    let numbers = [0, 255, 256, 65536, 16777216, u32::MAX - 1];
    for number in numbers {
        quota
            .alloc_inode(&mut io, &[ids(number)[0]], 1, false)
            .unwrap();
    }
    for (minimum, expected) in [
        (0, Some(0)),
        (1, Some(255)),
        (255, Some(255)),
        (257, Some(65536)),
        (65537, Some(16777216)),
        (16777217, Some(u32::MAX - 1)),
        (u32::MAX, None),
    ] {
        assert_eq!(
            quota
                .next_id(&mut io, QuotaType::User, minimum)
                .unwrap()
                .map(QuotaId::id),
            expected
        );
    }
}

#[def_test]
fn invalid_headers_fail_before_loading_or_writing_quota_state() {
    for (offset, value, errno) in [
        (0, 0, LinuxError::EINVAL),
        (4, 0, LinuxError::EINVAL),
        (20, 1, LinuxError::EUCLEAN),
        (20, u32::MAX, LinuxError::EUCLEAN),
        (24, 1, LinuxError::EUCLEAN),
        (28, 2, LinuxError::EUCLEAN),
    ] {
        let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
        io.put_u32(QuotaType::User, offset, value);
        let mut quota = QuotaInfo::new();
        assert_eq!(
            quota.enable(
                &mut io,
                QuotaType::User,
                &alloc::sync::Arc::new(QuotaType::User),
                QuotaFormat::VfsV1,
                QuotaState::USAGE_ENABLED
            ),
            Err(KError::from(errno))
        );
        assert!(quota.info(QuotaType::User).is_none());
        assert_eq!(io.writes.get(), 0);
    }
}

#[def_test]
fn tree_cycles_are_rejected_by_lookup_and_iteration() {
    for depth in 0..4 {
        let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
        let mut quota = io.enable_all(QuotaFormat::VfsV1);
        quota.get(&mut io, ids(0)[0]).unwrap();
        io.put_u32(QuotaType::User, (depth + 1) * 1024, 1);
        let mut reopened = io.enable_all(QuotaFormat::VfsV1);
        assert_eq!(
            reopened.get(&mut io, ids(0)[0]),
            Err(KError::from(LinuxError::EUCLEAN))
        );
        assert_eq!(
            reopened.next_id(&mut io, QuotaType::User, 0),
            Err(KError::from(LinuxError::EUCLEAN))
        );
    }
}

#[def_test]
fn referenced_but_missing_and_miscounted_records_are_corruption() {
    for has_corrupt_count in [false, true] {
        let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
        let mut quota = io.enable_all(QuotaFormat::VfsV1);
        quota.get(&mut io, ids(1)[0]).unwrap();
        if has_corrupt_count {
            io.files.borrow_mut()[0][5 * 1024 + 8] = 2;
        } else {
            io.put_u32(QuotaType::User, 5 * 1024 + 16, 2);
        }
        let mut reopened = io.enable_all(QuotaFormat::VfsV1);
        assert_eq!(
            reopened.get(&mut io, ids(1)[0]),
            Err(KError::from(LinuxError::EUCLEAN))
        );
    }
}

#[def_test]
fn free_block_self_cycles_are_rejected_before_the_next_allocation() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    let owner = ids(0)[0];
    quota.get(&mut io, owner).unwrap();
    quota.release(&mut io, owner).unwrap();
    let head = io.u32_at(QuotaType::User, 24);
    assert!(head > 1);
    io.put_u32(QuotaType::User, head as usize * 1024, head);
    let mut reopened = io.enable_all(QuotaFormat::VfsV1);
    let writes = io.writes.get();
    assert_eq!(
        reopened.get(&mut io, ids(1)[0]),
        Err(KError::from(LinuxError::EUCLEAN))
    );
    assert_eq!(io.writes.get(), writes);
}

#[def_test]
fn short_record_reads_fail_without_replacing_an_existing_cached_record() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    let owner = ids(1)[0];
    quota.alloc_inode(&mut io, &[owner], 1, false).unwrap();
    io.has_short_reads.set(true);
    assert_eq!(
        quota.get(&mut io, ids(2)[0]),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(quota.get(&mut io, owner).unwrap().used_inodes, 1);
}

#[def_test]
fn format_limit_ranges_are_validated_before_record_acquisition() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV0);
    let mut quota = io.enable_all(QuotaFormat::VfsV0);
    assert_eq!(
        quota.set(
            &mut io,
            ids(1)[0],
            Dqblk {
                inode_hard_limit: i64::from(u32::MAX) + 1,
                ..Dqblk::default()
            },
            QuotaFields::INODE_HARD,
            1
        ),
        Err(KError::from(LinuxError::ERANGE))
    );
    assert_eq!(io.writes.get(), 0);
}

#[def_test]
fn the_largest_native_space_limit_survives_quota_block_rounding() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    let owner = ids(1)[0];
    quota
        .set(
            &mut io,
            owner,
            Dqblk {
                space_hard_limit_bytes: i64::MAX,
                ..Dqblk::default()
            },
            QuotaFields::SPACE_HARD,
            1,
        )
        .unwrap();
    quota.release(&mut io, owner).unwrap();
    assert_eq!(
        quota.get(&mut io, owner).unwrap().space_hard_limit_bytes,
        i64::MAX
    );
}

#[def_test]
fn reads_and_updates_an_e2fsprogs_generated_v2_quota_file() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    io.files.borrow_mut()[0] = include_bytes!("fixtures/e2fsprogs-v2-user.bin").to_vec();
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    let owner = ids(0)[0];
    let data = quota.get(&mut io, owner).unwrap();
    assert_eq!(data.used_inodes, 2);
    assert_eq!(data.used_bytes, 13312);
    assert_eq!(
        quota.info(QuotaType::User).unwrap().space_grace_seconds(),
        604800
    );
    quota
        .alloc_space(&mut io, &[owner], 1024, 1, ChargeFlags::WARN)
        .unwrap();
    quota.sync(&mut io, QuotaType::User).unwrap();
    let files = io.files.borrow();
    assert_eq!(&files[0][5136 + 48..5136 + 56], &14336_u64.to_le_bytes());
}

#[def_test]
fn disable_releases_instantiated_but_unused_records() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    quota.get(&mut io, ids(0)[0]).unwrap();
    quota.disable(&mut io, QuotaType::User).unwrap();
    quota
        .enable(
            &mut io,
            QuotaType::User,
            &alloc::sync::Arc::new(QuotaType::User),
            QuotaFormat::VfsV1,
            QuotaState::USAGE_ENABLED,
        )
        .unwrap();
    assert_eq!(quota.next_id(&mut io, QuotaType::User, 0).unwrap(), None);
}

#[def_test]
fn releasing_a_record_can_retry_a_failed_file_info_write() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    let owner = ids(0)[0];
    quota.get(&mut io, owner).unwrap();
    io.fail_write_at.set(Some(8));
    assert_eq!(
        quota.release(&mut io, owner),
        Err(KError::from(LinuxError::EIO))
    );
    io.fail_write_at.set(None);
    quota.release(&mut io, owner).unwrap();
    let reopened = io.enable_all(QuotaFormat::VfsV1);
    assert_eq!(reopened.next_id(&mut io, QuotaType::User, 0).unwrap(), None);
}

#[def_test]
fn acquisition_does_not_publish_a_record_before_its_file_info_is_written() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    let owner = ids(0)[0];
    io.fail_write_at.set(Some(8));
    assert_eq!(
        quota.get(&mut io, owner),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(
        quota.sync(&mut io, QuotaType::User),
        Err(KError::from(LinuxError::EIO))
    );
    assert_eq!(
        quota.get(&mut io, owner),
        Err(KError::from(LinuxError::EIO))
    );
    io.fail_write_at.set(None);
    assert_eq!(quota.get(&mut io, owner).unwrap(), Dqblk::default());
    let mut reopened = io.enable_all(QuotaFormat::VfsV1);
    assert_eq!(reopened.get(&mut io, owner).unwrap(), Dqblk::default());
}

#[def_test]
fn free_block_and_partial_entry_heads_cannot_alias() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    io.files.borrow_mut()[0] = include_bytes!("fixtures/e2fsprogs-v2-user.bin").to_vec();
    io.put_u32(QuotaType::User, 24, 5);
    let mut quota = QuotaInfo::new();
    assert_eq!(
        quota.enable(
            &mut io,
            QuotaType::User,
            &alloc::sync::Arc::new(QuotaType::User),
            QuotaFormat::VfsV1,
            QuotaState::USAGE_ENABLED
        ),
        Err(KError::from(LinuxError::EUCLEAN))
    );
    assert_eq!(io.writes.get(), 0);
}

#[def_test]
fn unlink_rejects_a_head_with_a_nonzero_previous_link_before_writing() {
    let mut io = MemoryFiles::new(QuotaFormat::VfsV1);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    for id in 0..15 {
        quota.get(&mut io, ids(id)[0]).unwrap();
    }
    drop(quota);
    let head = io.u32_at(QuotaType::User, 28);
    assert_eq!(head, 6);
    io.put_u32(QuotaType::User, head as usize * 1024 + 4, 5);
    io.put_u32(QuotaType::User, 5 * 1024, head);
    let mut quota = io.enable_all(QuotaFormat::VfsV1);
    let owner = ids(14)[0];
    quota.get(&mut io, owner).unwrap();
    let before = io.files.borrow()[0].clone();
    let writes = io.writes.get();
    assert_eq!(
        quota.release(&mut io, owner),
        Err(KError::from(LinuxError::EUCLEAN))
    );
    assert_eq!(io.writes.get(), writes);
    assert_eq!(io.files.borrow()[0], before);
}

#[def_test]
fn failed_first_write_does_not_publish_a_full_data_block_head() {
    for (format, capacity) in [(QuotaFormat::VfsV0, 21_u32), (QuotaFormat::VfsV1, 14)] {
        let mut io = MemoryFiles::new(format);
        let mut quota = io.enable_all(format);
        for id in 1..capacity {
            quota.get(&mut io, ids(id)[0]).unwrap();
        }
        quota.sync(&mut io, QuotaType::User).unwrap();
        let data_block = io.u32_at(QuotaType::User, 28);
        let original = io.files.borrow()[0].clone();
        io.fail_write_at.set(Some(u64::from(data_block) * 1024));
        assert_eq!(
            quota.get(&mut io, ids(capacity)[0]),
            Err(KError::from(LinuxError::EIO))
        );
        assert_eq!(io.files.borrow()[0], original);
        assert_eq!(
            quota.sync(&mut io, QuotaType::User),
            Err(KError::from(LinuxError::EIO))
        );
        quota.sync(&mut io, QuotaType::User).unwrap();
        assert_eq!(io.files.borrow()[0], original);
        io.fail_write_at.set(None);
        quota.get(&mut io, ids(capacity)[0]).unwrap();
        assert_eq!(io.u32_at(QuotaType::User, 28), 0);
        assert_eq!(io.files.borrow()[0].len(), original.len());
    }
}

#[def_test]
fn record_bounds_cover_radix_splits_and_free_list_reuse() {
    use alloc::{collections::BTreeSet, sync::Arc, vec::Vec};

    use kerrno::KResult;

    use crate::QuotaIo;

    struct WriteTrace {
        io: MemoryFiles,
        blocks: BTreeSet<u64>,
    }

    impl QuotaIo for WriteTrace {
        type File = Arc<QuotaType>;

        fn read(&self, file: &Self::File, offset: u64, bytes: &mut [u8]) -> KResult<usize> {
            self.io.read(file, offset, bytes)
        }

        fn write(&mut self, file: &Self::File, offset: u64, bytes: &[u8]) -> KResult<usize> {
            for block in offset / 1024..=(offset + bytes.len() as u64 - 1) / 1024 {
                self.blocks.insert(block);
            }
            self.io.write(file, offset, bytes)
        }

        fn file_len(&self, file: &Self::File) -> KResult<u64> {
            self.io.file_len(file)
        }
    }

    for (format, capacity) in [(QuotaFormat::VfsV0, 21), (QuotaFormat::VfsV1, 14)] {
        let mut io = WriteTrace {
            io: MemoryFiles::new(format),
            blocks: BTreeSet::new(),
        };
        let mut quota = io.io.enable_all(format);
        let info = quota.info(QuotaType::User).unwrap();
        let write_bound = info.record_write_blocks_bound();
        let growth_bound = info.record_growth_bytes_bound();
        let owners: Vec<_> = (0..capacity * 3)
            .chain([255, 256, 65535, 65536, 0x00ff_fffe, 0xffff_fffe])
            .map(|id| ids(id)[0])
            .collect();
        for _ in 0..2 {
            for &owner in &owners {
                io.blocks.clear();
                let length = io.io.files.borrow()[0].len();
                quota.get(&mut io, owner).unwrap();
                quota.sync(&mut io, QuotaType::User).unwrap();
                assert!(io.blocks.len() <= write_bound as usize);
                assert!((io.io.files.borrow()[0].len() - length) as u64 <= growth_bound);
            }
            for &owner in owners.iter().rev() {
                io.blocks.clear();
                quota.release(&mut io, owner).unwrap();
                assert!(io.blocks.len() <= write_bound as usize);
            }
        }
    }
}
