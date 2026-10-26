// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use kerrno::LinuxError;
use ktime_types::SystemTime;
use kvfs::{
    FiemapExtentFlags, FiemapExtentInfo, FiemapExtentWriter, FiemapFlags, MetadataUpdate,
    NodePermission, RenameFlags, VfsError, VfsResult, XattrName, XattrSetFlags,
};
use unittest::def_test;

use super::{
    Ext4MountOptions, ExtentWalkQuery, applied_metadata_update, parse_xattr_name,
    supports_rename_flags, walk_file_extents, xattr_set_mode,
};
use crate::{
    BlockCount, BlockMapping, BlockMappingFlags, Ext4InodeStat, Ext4Timestamp, Ext4XattrNamespace,
    Ext4XattrSetMode, PhysicalBlock, superblock::Ext4StatFsMode,
};

#[def_test]
fn journal_errors_preserve_space_and_read_only_semantics() {
    for error in [
        crate::Ext4Error::NoSpace,
        crate::Ext4Error::InsufficientJournalCredits,
    ] {
        assert_eq!(
            super::into_vfs_err(error),
            VfsError::from(LinuxError::ENOSPC).canonicalize()
        );
    }
    assert_eq!(
        super::into_vfs_err(crate::Ext4Error::JournalAborted),
        VfsError::from(LinuxError::EROFS).canonicalize()
    );
}

#[def_test]
fn setattr_result_uses_post_encoding_inode_values() {
    let requested_timestamp = SystemTime::from_unix_parts(2_147_483_648, 123_456_789).unwrap();
    let applied_timestamp = Ext4Timestamp::new(i32::MAX as i64, 0);
    let requested = MetadataUpdate {
        mode: Some(NodePermission::from_bits_truncate(0o777)),
        owner: Some((1000, 1001)),
        atime: Some(requested_timestamp),
        mtime: Some(requested_timestamp),
        ctime: Some(requested_timestamp),
        ..Default::default()
    };
    let applied = applied_metadata_update(
        requested,
        Ext4InodeStat {
            mode: 0o100640,
            uid: 2000,
            gid: 2001,
            size: 0,
            blocks: 0,
            rdev: None,
            links_count: 1,
            atime: applied_timestamp,
            ctime: applied_timestamp,
            mtime: applied_timestamp,
        },
    );
    let applied_system_time = SystemTime::from_unix_seconds(i32::MAX as i64);

    assert_eq!(applied.mode.map(|mode| mode.bits()), Some(0o640));
    assert_eq!(applied.owner, Some((2000, 2001)));
    assert_eq!(applied.atime, Some(applied_system_time));
    assert_eq!(applied.mtime, Some(applied_system_time));
    assert_eq!(applied.ctime, Some(applied_system_time));
}

#[def_test]
fn statfs_mount_options_default_to_bsddf() {
    assert_eq!(Ext4MountOptions::parse(None).unwrap().statfs_mode, None);
    assert_eq!(
        Ext4MountOptions::parse(None).unwrap().mount_statfs_mode(),
        Ext4StatFsMode::Bsd
    );
    assert_eq!(
        Ext4MountOptions::parse(Some(b"\0"))
            .unwrap()
            .mount_statfs_mode(),
        Ext4StatFsMode::Bsd
    );
}

#[def_test]
fn last_statfs_mount_option_wins() {
    assert_eq!(
        Ext4MountOptions::parse(Some(b"bsddf,minixdf\0"))
            .unwrap()
            .mount_statfs_mode(),
        Ext4StatFsMode::Minix
    );
    assert_eq!(
        Ext4MountOptions::parse(Some(b"minixdf,,bsddf\0"))
            .unwrap()
            .mount_statfs_mode(),
        Ext4StatFsMode::Bsd
    );
}

#[def_test]
fn linux_default_acl_and_user_xattr_options_are_accepted() {
    for option in [
        &b"acl\0"[..],
        &b"user_xattr\0"[..],
        &b"acl,user_xattr\0"[..],
    ] {
        assert_eq!(
            Ext4MountOptions::parse(Some(option)).unwrap(),
            Ext4MountOptions::default()
        );
    }
    assert_eq!(
        Ext4MountOptions::parse(Some(b"acl,minixdf,user_xattr\0"))
            .unwrap()
            .mount_statfs_mode(),
        Ext4StatFsMode::Minix
    );
}

#[def_test]
fn unsupported_mount_options_are_rejected() {
    for option in [&b"garbage\0"[..], &b"discard\0"[..], &b"nodelalloc\0"[..]] {
        assert_eq!(
            Ext4MountOptions::parse(Some(option)).unwrap_err(),
            VfsError::InvalidInput
        );
    }
}

#[def_test]
fn bytes_after_mount_data_terminator_are_ignored() {
    assert_eq!(
        Ext4MountOptions::parse(Some(b"minixdf\0\xff"))
            .unwrap()
            .mount_statfs_mode(),
        Ext4StatFsMode::Minix
    );
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CollectedExtent {
    logical: u64,
    physical: u64,
    length: u64,
    flags: FiemapExtentFlags,
}

#[derive(Default)]
struct CollectingWriter {
    extents: Vec<CollectedExtent>,
}

impl FiemapExtentWriter for CollectingWriter {
    fn write_extent(
        &mut self,
        index: u32,
        logical: u64,
        physical: u64,
        length: u64,
        flags: FiemapExtentFlags,
    ) -> VfsResult<()> {
        assert_eq!(usize::try_from(index).ok(), Some(self.extents.len()));
        self.extents.push(CollectedExtent {
            logical,
            physical,
            length,
            flags,
        });
        Ok(())
    }
}

fn collect_extents(
    query: ExtentWalkQuery,
    capacity: u32,
    map_blocks: impl FnMut(u64) -> VfsResult<BlockMapping>,
) -> VfsResult<Vec<CollectedExtent>> {
    let mut writer = CollectingWriter::default();
    {
        let mut info = FiemapExtentInfo::new(FiemapFlags::empty(), capacity, &mut writer);
        walk_file_extents(query, map_blocks, &mut info)?;
    }
    Ok(writer.extents)
}
#[def_test]
fn rename_support_is_limited_to_move_and_noreplace() {
    assert!(supports_rename_flags(RenameFlags::empty()));
    assert!(supports_rename_flags(RenameFlags::NOREPLACE));
    assert!(!supports_rename_flags(RenameFlags::EXCHANGE));
    assert!(!supports_rename_flags(RenameFlags::WHITEOUT));
}

#[def_test]
fn fiemap_reports_mapped_and_unwritten_extents_but_skips_holes() {
    let extents = collect_extents(
        ExtentWalkQuery {
            start_block: 0,
            end_block: 6,
            block_size: 4096,
        },
        u32::MAX,
        |logical| match logical {
            0 => Ok(BlockMapping::Mapped {
                physical: PhysicalBlock::new(10),
                len: BlockCount::new(2),
                flags: BlockMappingFlags::empty(),
            }),
            2 => Ok(BlockMapping::Hole {
                len: BlockCount::new(2),
                flags: BlockMappingFlags::empty(),
            }),
            4 => Ok(BlockMapping::Unwritten {
                physical: PhysicalBlock::new(20),
                len: BlockCount::new(2),
                flags: BlockMappingFlags::empty(),
            }),
            _ => unreachable!("the mapping walk should advance by complete runs"),
        },
    )
    .expect("fiemap walk should succeed");

    assert_eq!(extents.len(), 2);
    assert_eq!(extents[0].logical, 0);
    assert_eq!(extents[0].physical, 10 * 4096);
    assert_eq!(extents[0].length, 2 * 4096);
    assert!(extents[0].flags.is_empty());
    assert_eq!(extents[1].logical, 4 * 4096);
    assert!(extents[1].flags.contains(FiemapExtentFlags::UNWRITTEN));
    assert!(extents[1].flags.contains(FiemapExtentFlags::LAST));
}

#[def_test]
fn fiemap_reports_delayed_allocation_with_unknown_location() {
    let extents = collect_extents(
        ExtentWalkQuery {
            start_block: 0,
            end_block: 6,
            block_size: 4096,
        },
        u32::MAX,
        |logical| match logical {
            0 => Ok(BlockMapping::Hole {
                len: BlockCount::new(2),
                flags: BlockMappingFlags::empty(),
            }),
            2 => Ok(BlockMapping::Hole {
                len: BlockCount::new(2),
                flags: BlockMappingFlags::DELAYED,
            }),
            4 => Ok(BlockMapping::Hole {
                len: BlockCount::new(2),
                flags: BlockMappingFlags::empty(),
            }),
            _ => unreachable!("the mapping walk should advance by complete runs"),
        },
    )
    .expect("delayed mapping walk should succeed");

    assert_eq!(extents.len(), 1);
    assert_eq!(extents[0].logical, 2 * 4096);
    assert_eq!(extents[0].length, 2 * 4096);
    assert!(
        extents[0]
            .flags
            .contains(FiemapExtentFlags::DELALLOC | FiemapExtentFlags::UNKNOWN)
    );
    assert!(extents[0].flags.contains(FiemapExtentFlags::LAST));
}

#[def_test]
fn fiemap_reports_separate_delayed_runs() {
    let extents = collect_extents(
        ExtentWalkQuery {
            start_block: 0,
            end_block: 4,
            block_size: 4096,
        },
        u32::MAX,
        |logical| match logical {
            0 | 2 => Ok(BlockMapping::Hole {
                len: BlockCount::new(1),
                flags: BlockMappingFlags::empty(),
            }),
            1 | 3 => Ok(BlockMapping::Hole {
                len: BlockCount::new(1),
                flags: BlockMappingFlags::DELAYED,
            }),
            _ => unreachable!("the mapping walk should advance by complete runs"),
        },
    )
    .expect("delayed mappings inside one hole should succeed");

    assert_eq!(extents.len(), 2);
    assert_eq!(extents[0].logical, 4096);
    assert_eq!(extents[1].logical, 3 * 4096);
}

#[def_test]
fn fiemap_does_not_mark_a_full_partial_result_as_last() {
    let extents = collect_extents(
        ExtentWalkQuery {
            start_block: 0,
            end_block: 3,
            block_size: 4096,
        },
        1,
        |logical| match logical {
            0 => Ok(BlockMapping::Mapped {
                physical: PhysicalBlock::new(10),
                len: BlockCount::new(1),
                flags: BlockMappingFlags::empty(),
            }),
            1 => Ok(BlockMapping::Hole {
                len: BlockCount::new(1),
                flags: BlockMappingFlags::empty(),
            }),
            2 => Ok(BlockMapping::Mapped {
                physical: PhysicalBlock::new(20),
                len: BlockCount::new(1),
                flags: BlockMappingFlags::empty(),
            }),
            _ => unreachable!("the mapping walk should stop at the query end"),
        },
    )
    .expect("bounded fiemap walk should succeed");

    assert_eq!(extents.len(), 1);
    assert!(!extents[0].flags.contains(FiemapExtentFlags::LAST));
}

#[def_test]
fn fiemap_merges_contiguous_legacy_block_runs() {
    let extents = collect_extents(
        ExtentWalkQuery {
            start_block: 0,
            end_block: 4,
            block_size: 4096,
        },
        u32::MAX,
        |logical| match logical {
            0 => Ok(BlockMapping::Mapped {
                physical: PhysicalBlock::new(10),
                len: BlockCount::new(2),
                flags: BlockMappingFlags::MERGED,
            }),
            2 => Ok(BlockMapping::Mapped {
                physical: PhysicalBlock::new(12),
                len: BlockCount::new(2),
                flags: BlockMappingFlags::MERGED,
            }),
            _ => unreachable!("the mapping walk should advance by complete runs"),
        },
    )
    .expect("legacy fiemap walk should succeed");

    assert_eq!(extents.len(), 1);
    assert_eq!(extents[0].length, 4 * 4096);
    assert!(
        extents[0]
            .flags
            .contains(FiemapExtentFlags::MERGED | FiemapExtentFlags::LAST)
    );
}

#[def_test]
fn fiemap_clips_mapping_runs_to_the_query_end() {
    let extents = collect_extents(
        ExtentWalkQuery {
            start_block: 0,
            end_block: 2,
            block_size: 4096,
        },
        u32::MAX,
        |_| {
            Ok(BlockMapping::Mapped {
                physical: PhysicalBlock::new(10),
                len: BlockCount::new(8),
                flags: BlockMappingFlags::empty(),
            })
        },
    )
    .expect("bounded fiemap walk should succeed");

    assert_eq!(extents.len(), 1);
    assert_eq!(extents[0].length, 2 * 4096);
    assert!(extents[0].flags.contains(FiemapExtentFlags::LAST));
}

#[def_test]
fn xattr_name_mapping_accepts_public_ext4_namespaces() {
    for (name, namespace) in [
        (&b"user.key"[..], Ext4XattrNamespace::User),
        (&b"trusted.key"[..], Ext4XattrNamespace::Trusted),
        (&b"security.key"[..], Ext4XattrNamespace::Security),
    ] {
        let name = XattrName::new(name.to_vec()).unwrap();
        assert_eq!(parse_xattr_name(&name), Ok((namespace, &b"key"[..])));
    }

    for name in [&b"user."[..], &b"trusted."[..], &b"security."[..]] {
        let name = XattrName::new(name.to_vec()).unwrap();
        assert!(matches!(
            parse_xattr_name(&name),
            Err(err) if LinuxError::from(err) == LinuxError::EINVAL
        ));
    }

    let acl = XattrName::new(b"system.posix_acl_access".to_vec()).unwrap();
    assert!(matches!(
        parse_xattr_name(&acl),
        Err(err) if LinuxError::from(err) == LinuxError::EOPNOTSUPP
    ));
}

#[def_test]
fn xattr_set_flags_preserve_all_four_combinations() {
    assert_eq!(
        xattr_set_mode(XattrSetFlags::empty()),
        Ext4XattrSetMode::CreateOrReplace
    );
    assert_eq!(
        xattr_set_mode(XattrSetFlags::CREATE),
        Ext4XattrSetMode::Create
    );
    assert_eq!(
        xattr_set_mode(XattrSetFlags::REPLACE),
        Ext4XattrSetMode::Replace
    );
    assert_eq!(
        xattr_set_mode(XattrSetFlags::CREATE | XattrSetFlags::REPLACE),
        Ext4XattrSetMode::CreateAndReplace
    );
}
