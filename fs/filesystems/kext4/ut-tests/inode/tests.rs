// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{
    EncodedTimestamp, Ext4DeviceId, Ext4Inode, Ext4InodeMetadata, Ext4Timestamp,
    InodeInitialization, InodeKind, RawTimestampFields, disk_inode,
};
use crate::{CorruptKind, Ext4Error, InodeNumber, UnsupportedKind};

const BLOCK_SIZE: u32 = 4096;

#[def_test]
fn exposes_immutable_and_append_only_inode_flags() {
    let inode = symlink_inode(
        9,
        0,
        disk_inode::EXT4_IMMUTABLE_FL | disk_inode::EXT4_APPEND_FL,
        0,
        fast_symlink_block(b"hello.txt"),
    );

    assert!(inode.is_immutable());
    assert!(inode.is_append_only());
}

#[def_test]
fn classifies_fast_symlink_by_data_blocks_not_size_only() {
    let block = fast_symlink_block(b"hello.txt");
    let inode = symlink_inode(9, 0, 0, 0, block);

    let storage = inode
        .fast_symlink_target(BLOCK_SIZE, false)
        .expect("classify symlink storage");

    assert_eq!(storage.as_deref(), Some(b"hello.txt".as_slice()));
}

#[def_test]
fn classifies_short_block_mapped_symlink_as_block_mapped() {
    let inode = symlink_inode(9, 8, disk_inode::EXT4_EXTENTS_FL, 0, [0; 60]);

    let storage = inode
        .fast_symlink_target(BLOCK_SIZE, false)
        .expect("classify symlink storage");

    assert_eq!(storage, None);
}

#[def_test]
fn excludes_xattr_blocks_from_fast_symlink_data_blocks() {
    let block = fast_symlink_block(b"hello.txt");
    let inode = symlink_inode(9, 8, 0, 42, block);

    let storage = inode
        .fast_symlink_target(BLOCK_SIZE, false)
        .expect("classify symlink storage");

    assert_eq!(storage.as_deref(), Some(b"hello.txt".as_slice()));
}

#[def_test]
fn classifies_symlink_with_filesystem_ea_inode_feature_not_inode_flag() {
    let block = fast_symlink_block(b"hello.txt");
    let inode = symlink_inode(9, 8, 0, 0, block);

    let storage = inode
        .fast_symlink_target(BLOCK_SIZE, true)
        .expect("classify symlink storage");

    assert_eq!(storage.as_deref(), Some(b"hello.txt".as_slice()));
}

#[def_test]
fn ignores_inode_ea_inode_flag_when_filesystem_feature_is_absent() {
    let inode = symlink_inode(9, 8, disk_inode::EXT4_EA_INODE_FL, 0, [0; 60]);

    let storage = inode
        .fast_symlink_target(BLOCK_SIZE, false)
        .expect("classify symlink storage");

    assert_eq!(storage, None);
}

#[def_test]
fn rejects_zero_length_fast_symlink() {
    let inode = symlink_inode(0, 0, 0, 0, fast_symlink_block(b"hello.txt"));

    assert_eq!(
        inode.fast_symlink_target(BLOCK_SIZE, false),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
    );
}

#[def_test]
fn rejects_fast_symlink_size_that_reaches_past_i_block() {
    let inode = symlink_inode(60, 0, 0, 0, [b'a'; disk_inode::INODE_BLOCK_BYTES]);

    assert_eq!(
        inode.fast_symlink_target(BLOCK_SIZE, false),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
    );
}

#[def_test]
fn rejects_fast_symlink_without_nul_terminator_at_size() {
    let mut block = [b'a'; disk_inode::INODE_BLOCK_BYTES];
    block[..9].copy_from_slice(b"hello.txt");
    let inode = symlink_inode(9, 0, 0, 0, block);

    assert_eq!(
        inode.fast_symlink_target(BLOCK_SIZE, false),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
    );
}

#[def_test]
fn rejects_fast_symlink_with_embedded_nul_before_size() {
    let mut block = fast_symlink_block(b"hello.txt");
    block[4] = 0;
    let inode = symlink_inode(9, 0, 0, 0, block);

    assert_eq!(
        inode.fast_symlink_target(BLOCK_SIZE, false),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
    );
}

#[def_test]
fn decodes_old_device_id_from_i_block_zero() {
    let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
    put_u32(&mut block, 0, (12 << 8) | 34);
    let inode = special_inode(InodeKind::CharacterDevice, disk_inode::S_IFCHR, block);

    let device = inode.device_id().expect("character device has rdev");

    assert_eq!(device.major(), 12);
    assert_eq!(device.minor(), 34);
}

#[def_test]
fn decodes_new_device_id_from_i_block_one() {
    let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
    put_u32(&mut block, 4, encode_new_device_id(0xabc, 0xdef0));
    let inode = special_inode(InodeKind::BlockDevice, disk_inode::S_IFBLK, block);

    let device = inode.device_id().expect("block device has rdev");

    assert_eq!(device.major(), 0xabc);
    assert_eq!(device.minor(), 0xdef0);
}

#[def_test]
fn rejects_device_id_outside_ext4_new_encoding() {
    assert_eq!(
        Ext4DeviceId::new_checked(0x1000, 0),
        Err(Ext4Error::Unsupported(UnsupportedKind::DeviceId))
    );
    assert_eq!(
        Ext4DeviceId::new_checked(0, 0x0010_0000),
        Err(Ext4Error::Unsupported(UnsupportedKind::DeviceId))
    );
    assert!(Ext4DeviceId::new_checked(0x0fff, 0x000f_ffff).is_ok());
}

#[def_test]
fn special_inode_initialization_rejects_unencodable_device_id() {
    assert_eq!(
        InodeInitialization::special(
            InodeKind::CharacterDevice,
            0o666,
            Some(Ext4DeviceId::new(0x1000, 0)),
            0,
            0,
        ),
        Err(Ext4Error::Unsupported(UnsupportedKind::DeviceId))
    );
}
#[def_test]
fn decodes_signed_timestamp_without_extra_epoch() {
    let timestamp =
        Ext4Timestamp::try_from(timestamp_fields(0xffff_ffff, None)).expect("decode timestamp");

    assert_eq!(timestamp, Ext4Timestamp::new(-1, 0));
}

#[def_test]
fn decodes_timestamp_with_extra_epoch_and_nanoseconds() {
    let timestamp =
        Ext4Timestamp::try_from(timestamp_fields(0x8000_0000, Some((123_456_789 << 2) | 1)))
            .expect("decode timestamp");

    assert_eq!(timestamp, Ext4Timestamp::new(2_147_483_648, 123_456_789));
}

#[def_test]
fn rejects_timestamp_with_invalid_nanoseconds() {
    assert_eq!(
        Ext4Timestamp::try_from(timestamp_fields(0, Some(1_000_000_000 << 2))),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidInode))
    );
}

#[def_test]
fn encodes_timestamp_without_extra_field() {
    let encoded = Ext4Timestamp::new(-1, 0)
        .encode(false)
        .expect("encode timestamp");

    assert_eq!(
        encoded,
        EncodedTimestamp {
            base_seconds: 0xffff_ffff,
            extra: None,
        }
    );
}

#[def_test]
fn truncates_nanoseconds_without_extra_timestamp_field() {
    let encoded = Ext4Timestamp::new(1, 1)
        .encode(false)
        .expect("encode legacy timestamp");

    assert_eq!(
        encoded,
        EncodedTimestamp {
            base_seconds: 1,
            extra: None,
        }
    );
}

#[def_test]
fn clamps_seconds_without_extra_timestamp_field() {
    let encoded = Ext4Timestamp::new(i64::MAX, 0)
        .encode(false)
        .expect("encode legacy timestamp");

    assert_eq!(
        encoded,
        EncodedTimestamp {
            base_seconds: i32::MAX as u32,
            extra: None,
        }
    );
}

#[def_test]
fn encodes_timestamp_with_extra_epoch_and_nanoseconds() {
    let encoded = Ext4Timestamp::new(2_147_483_648, 123_456_789)
        .encode(true)
        .expect("encode timestamp");

    assert_eq!(
        encoded,
        EncodedTimestamp {
            base_seconds: 0x8000_0000,
            extra: Some((123_456_789 << 2) | 1),
        }
    );
}

fn symlink_inode(
    size: u64,
    blocks: u64,
    flags: u32,
    file_acl: u64,
    block: [u8; disk_inode::INODE_BLOCK_BYTES],
) -> Ext4Inode {
    test_inode_with_flags(
        flags,
        disk_inode::S_IFLNK,
        Ext4InodeMetadata {
            uid: 0,
            gid: 0,
            project_id: 0,
            disk_size: size,
            blocks,
            block,
            file_acl,
            extra_isize: 0,
            inline_xattr: alloc::vec::Vec::new(),
            generation: 0,
            links_count: 1,
            atime: timestamp(0, 0),
            ctime: timestamp(0, 0),
            mtime: timestamp(0, 0),
        },
    )
}

fn special_inode(
    kind: InodeKind,
    mode: u16,
    block: [u8; disk_inode::INODE_BLOCK_BYTES],
) -> Ext4Inode {
    let metadata = Ext4InodeMetadata {
        uid: 0,
        gid: 0,
        project_id: 0,
        disk_size: 0,
        blocks: 0,
        block,
        file_acl: 0,
        extra_isize: 0,
        inline_xattr: alloc::vec::Vec::new(),
        generation: 0,
        links_count: 1,
        atime: timestamp(0, 0),
        ctime: timestamp(0, 0),
        mtime: timestamp(0, 0),
    };
    Ext4Inode::new(InodeNumber::new(12), kind, 0, mode & 0o7777, metadata)
}

fn test_inode(metadata: Ext4InodeMetadata) -> Ext4Inode {
    test_inode_with_flags(disk_inode::EXT4_EXTENTS_FL, disk_inode::S_IFREG | 0o644, metadata)
}

fn test_inode_with_flags(flags: u32, mode: u16, metadata: Ext4InodeMetadata) -> Ext4Inode {
    let kind = InodeKind::from_mode(mode).expect("test inode mode decodes");
    Ext4Inode::new(InodeNumber::new(12), kind, flags, mode & 0o7777, metadata)
}

fn regular_inode_metadata() -> Ext4InodeMetadata {
    Ext4InodeMetadata {
        uid: 0,
        gid: 0,
        project_id: 0,
        disk_size: 0,
        blocks: 0,
        block: [0; disk_inode::INODE_BLOCK_BYTES],
        file_acl: 0,
        extra_isize: 0,
        inline_xattr: alloc::vec::Vec::new(),
        generation: 1,
        links_count: 1,
        atime: timestamp(0, 0),
        ctime: timestamp(0, 0),
        mtime: timestamp(0, 0),
    }
}

#[def_test]
fn buffered_write_preparation_preserves_timestamps_on_size_rejection() {
    for has_extents in [false, true] {
        let metadata = regular_inode_metadata();
        let flags = if has_extents {
            disk_inode::EXT4_EXTENTS_FL
        } else {
            0
        };
        let inode = test_inode_with_flags(flags, disk_inode::S_IFREG | 0o644, metadata);
        let before = (inode.atime(), inode.mtime(), inode.ctime());
        let limit = if has_extents { 8192 } else { 4096 };
        let written_at = timestamp(123, 456);
        assert_eq!(
            inode.prepare_buffered_write(limit, 1, 8192, 4096, written_at),
            None
        );
        assert_eq!(
            inode.prepare_buffered_write(u64::MAX, 0, 8192, 4096, written_at),
            Some(0)
        );
        assert_eq!((inode.atime(), inode.mtime(), inode.ctime()), before);
        assert!(!inode.has_dirty_timestamps());
        assert_eq!(
            inode.prepare_buffered_write(limit - 3, usize::MAX, 8192, 4096, written_at),
            Some(3)
        );
        assert_eq!(inode.mtime(), written_at);
        assert_eq!(inode.ctime(), written_at);
        assert_eq!(inode.atime(), timestamp(0, 0));
        assert!(inode.has_dirty_timestamps());
    }
}

#[def_test]
fn extra_isize_expansion_failure_state_is_resident_and_resettable() {
    let inode = test_inode(regular_inode_metadata());
    assert!(!inode.is_extra_isize_expansion_disabled());

    inode.disable_extra_isize_expansion();
    let mut updated = regular_inode_metadata();
    updated.extra_isize = 16;
    inode.publish_metadata(updated).unwrap();
    assert!(inode.is_extra_isize_expansion_disabled());

    inode.enable_extra_isize_expansion();
    assert!(!inode.is_extra_isize_expansion_disabled());
}

#[def_test]
fn buffered_write_repetition_keeps_resident_timestamps_idempotent() {
    let inode = test_inode_with_flags(0, disk_inode::S_IFREG | 0o644, regular_inode_metadata());
    let written_at = timestamp(123, 456);
    assert_eq!(
        inode.prepare_buffered_write(10, 4, 8192, 4096, written_at),
        Some(4)
    );
    assert_eq!((inode.mtime(), inode.ctime()), (written_at, written_at));
    assert!(inode.has_dirty_timestamps());

    // Repeating the same coarse timestamp is a no-op on the resident pair.
    // The skipped write section has no observable state, so this test pins
    // the idempotence the skip must preserve.
    assert_eq!(
        inode.prepare_buffered_write(10, 4, 8192, 4096, written_at),
        Some(4)
    );
    assert_eq!((inode.mtime(), inode.ctime()), (written_at, written_at));
    assert!(inode.has_dirty_timestamps());
}

#[def_test]
fn needs_write_timestamp_compares_both_resident_fields() {
    let inode = test_inode_with_flags(0, disk_inode::S_IFREG | 0o644, regular_inode_metadata());
    let published = timestamp(123, 456);
    assert!(inode.with_state(|state| state.needs_write_timestamp(published)));

    assert_eq!(
        inode.prepare_buffered_write(10, 4, 8192, 4096, published),
        Some(4)
    );
    assert!(!inode.with_state(|state| state.needs_write_timestamp(published)));

    inode.set_mtime(timestamp(1, 2));
    assert!(inode.with_state(|state| state.needs_write_timestamp(published)));

    // The ctime arm of the comparison is independent of the mtime arm.
    inode.set_mtime(published);
    inode.set_ctime(timestamp(3, 4));
    assert!(inode.with_state(|state| state.needs_write_timestamp(published)));
}

#[def_test]
fn delayed_allocation_extents_merge_split_and_count_blocks() {
    let inode = test_inode(regular_inode_metadata());
    assert_eq!(
        inode.insert_unreserved_delalloc_extents(&[(3, 8)]).unwrap(),
        5
    );
    assert_eq!(
        inode
            .insert_unreserved_delalloc_extents(&[(10, 12)])
            .unwrap(),
        2
    );
    assert_eq!(
        inode
            .insert_unreserved_delalloc_extents(&[(8, 10)])
            .unwrap(),
        2
    );
    assert_eq!(
        inode.remove_delalloc_extent_with(5, 10, |_, _| Err(Ext4Error::Overflow)),
        Err(Ext4Error::Overflow)
    );
    assert_eq!(inode.reserved_data_blocks(), 9);
    assert_eq!(inode.remove_delalloc_extent(5, 10).unwrap(), 5);
    assert_eq!(
        inode.unreserved_delalloc_extents(0, 13),
        alloc::vec![(0, 3), (5, 10), (12, 13)]
    );
    assert_eq!(inode.remove_delalloc_from(4).unwrap(), 3);
    assert_eq!(inode.clear_delalloc_reservations(), 1);
    assert!(!inode.has_delalloc_reservations());
}

const fn timestamp(seconds: i64, nanos: u32) -> Ext4Timestamp {
    Ext4Timestamp::new(seconds, nanos)
}

const fn timestamp_fields(base_seconds: u32, extra: Option<u32>) -> RawTimestampFields {
    RawTimestampFields::new(base_seconds, extra)
}

fn fast_symlink_block(target: &[u8]) -> [u8; disk_inode::INODE_BLOCK_BYTES] {
    let mut block = [0; disk_inode::INODE_BLOCK_BYTES];
    block[..target.len()].copy_from_slice(target);
    block
}

fn encode_new_device_id(major: u32, minor: u32) -> u32 {
    (minor & 0xff) | (major << 8) | ((minor & !0xff) << 12)
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

#[def_test]
fn invalid_delalloc_extents_do_not_publish_a_valid_prefix_or_counter_change() {
    let inode = test_inode(regular_inode_metadata());
    inode.insert_unreserved_delalloc_extents(&[(3, 8)]).unwrap();
    for extents in [&[(9, 10), (7, 8)][..], &[(9, 10), (11, 11)][..]] {
        assert_eq!(
            inode.insert_unreserved_delalloc_extents(extents),
            Err(Ext4Error::InvalidDelayedAllocationState)
        );
        assert_eq!(inode.reserved_data_blocks(), 5);
        assert_eq!(
            inode.unreserved_delalloc_extents(0, 12),
            alloc::vec![(0, 3), (8, 12)]
        );
    }
}
