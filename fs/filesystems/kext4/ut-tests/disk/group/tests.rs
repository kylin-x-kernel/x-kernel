// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{
    BlockGroupDescriptor, EXT4_BG_BLOCK_UNINIT, EXT4_BG_INODE_UNINIT,
    decrement_group_free_blocks_count, increment_group_free_blocks_count,
    update_group_block_bitmap_metadata, update_group_inode_allocation_metadata,
    update_group_inode_bitmap_metadata,
};
use crate::disk::checksum;

#[def_test]
fn decrements_64bit_group_free_blocks_and_updates_checksum() {
    let seed = 0x1234_5678;
    let group = 7;
    let mut bytes = valid_group_descriptor();
    put_u16(&mut bytes, 12, 0x0001);
    put_u16(&mut bytes, 44, 0x0001);
    update_group_checksum(&mut bytes, group, seed);

    let descriptor =
        decrement_group_free_blocks_count(&mut bytes, group, seed, true, true, 2).unwrap();

    assert_eq!(descriptor.free_blocks_count(), 0xffff);
    assert_eq!(
        BlockGroupDescriptor::decode(&bytes, true).unwrap(),
        descriptor
    );
    assert_eq!(
        descriptor.checksum(),
        checksum::group_descriptor_checksum(&bytes, group, seed).unwrap()
    );
}

#[def_test]
fn increment_rejects_counter_overflow() {
    let mut bytes = valid_group_descriptor();
    put_u16(&mut bytes, 12, 0xffff);
    put_u16(&mut bytes, 44, 0xffff);

    assert_eq!(
        increment_group_free_blocks_count(&mut bytes, 0, 0, true, false, 1),
        Err(crate::Ext4Error::Overflow)
    );
}

#[def_test]
fn non_64bit_counter_rejects_truncated_high_bits() {
    let mut bytes = valid_group_descriptor();
    put_u16(&mut bytes, 12, 0xffff);

    assert_eq!(
        increment_group_free_blocks_count(&mut bytes, 0, 0, false, false, 1),
        Err(crate::Ext4Error::Overflow)
    );
}

#[def_test]
fn decodes_bitmap_checksums_and_itable_unused() {
    let bytes = descriptor_with_bitmap_metadata();

    let descriptor = BlockGroupDescriptor::decode(&bytes, true).unwrap();

    assert_eq!(descriptor.block_bitmap_checksum(), 0x1234_5678);
    assert_eq!(descriptor.inode_bitmap_checksum(), 0x9abc_def0);
    assert_eq!(descriptor.itable_unused_count(), 0x0002_0003);
}

#[def_test]
fn updates_block_bitmap_checksum_and_clears_uninit_flag() {
    let seed = 0x1122_3344;
    let group = 2;
    let bitmap = [0b1010_0101; 4096];
    let mut bytes = valid_group_descriptor();
    put_u16(&mut bytes, 18, EXT4_BG_BLOCK_UNINIT);

    let descriptor = update_group_block_bitmap_metadata(
        &mut bytes,
        group,
        seed,
        true,
        true,
        bitmap.len(),
        &bitmap,
    )
    .unwrap();

    assert_eq!(descriptor.flags() & EXT4_BG_BLOCK_UNINIT, 0);
    assert_eq!(
        descriptor.block_bitmap_checksum(),
        checksum::bitmap_checksum(&bitmap, seed)
    );
    assert_eq!(
        descriptor.checksum(),
        checksum::group_descriptor_checksum(&bytes, group, seed).unwrap()
    );
}

#[def_test]
fn updates_inode_bitmap_checksum_and_clears_uninit_flag() {
    let seed = 0x5566_7788;
    let group = 3;
    let bitmap = [0b0101_1010; 4096];
    let mut bytes = valid_group_descriptor();
    put_u16(&mut bytes, 18, EXT4_BG_INODE_UNINIT);

    let descriptor = update_group_inode_bitmap_metadata(
        &mut bytes,
        group,
        seed,
        true,
        true,
        bitmap.len(),
        &bitmap,
    )
    .unwrap();

    assert_eq!(descriptor.flags() & EXT4_BG_INODE_UNINIT, 0);
    assert_eq!(
        descriptor.inode_bitmap_checksum(),
        checksum::bitmap_checksum(&bitmap, seed)
    );
    assert_eq!(
        descriptor.checksum(),
        checksum::group_descriptor_checksum(&bytes, group, seed).unwrap()
    );
}

#[def_test]
fn inode_allocation_reduces_itable_unused_tail() {
    let seed = 0x99aa_bbcc;
    let group = 4;
    let bitmap = [0; 4096];
    let mut bytes = valid_group_descriptor();
    put_u16(&mut bytes, 18, EXT4_BG_INODE_UNINIT);
    put_u16(&mut bytes, 28, 32);

    let descriptor = update_group_inode_allocation_metadata(
        &mut bytes,
        group,
        seed,
        true,
        true,
        bitmap.len(),
        &bitmap,
        10,
        32,
        true,
    )
    .unwrap();

    assert_eq!(descriptor.itable_unused_count(), 21);
    assert_eq!(
        descriptor.inode_bitmap_checksum(),
        checksum::bitmap_checksum(&bitmap, seed)
    );
}

#[def_test]
fn inode_allocation_treats_uninit_inode_bitmap_as_fully_unused() {
    let seed = 0x99aa_bbcc;
    let group = 4;
    let bitmap = [0; 4096];
    let mut bytes = valid_group_descriptor();
    put_u16(&mut bytes, 18, EXT4_BG_INODE_UNINIT);
    put_u16(&mut bytes, 28, 3);

    let descriptor = update_group_inode_allocation_metadata(
        &mut bytes,
        group,
        seed,
        true,
        true,
        bitmap.len(),
        &bitmap,
        10,
        32,
        true,
    )
    .unwrap();

    assert_eq!(descriptor.itable_unused_count(), 21);
}

#[def_test]
fn checked_u16_write_reports_truncated_descriptor() {
    let mut bytes = [0; 1];

    assert_eq!(
        super::put_u16_at(&mut bytes, 0, 1),
        Err(crate::Ext4Error::Corrupt(crate::CorruptKind::Truncated))
    );
}

#[def_test]
fn checked_u16_write_reports_offset_overflow() {
    let mut bytes = [0; 8];

    assert_eq!(
        super::put_u16_at(&mut bytes, usize::MAX, 1),
        Err(crate::Ext4Error::Overflow)
    );
}

fn valid_group_descriptor() -> [u8; 64] {
    let mut bytes = [0; 64];
    put_u32(&mut bytes, 0, 8);
    put_u32(&mut bytes, 4, 9);
    put_u32(&mut bytes, 8, 10);
    bytes
}

fn descriptor_with_bitmap_metadata() -> [u8; 64] {
    let mut bytes = valid_group_descriptor();
    put_u16(&mut bytes, 24, 0x5678);
    put_u16(&mut bytes, 26, 0xdef0);
    put_u16(&mut bytes, 28, 0x0003);
    put_u16(&mut bytes, 50, 0x0002);
    put_u16(&mut bytes, 56, 0x1234);
    put_u16(&mut bytes, 58, 0x9abc);
    bytes
}

fn update_group_checksum(bytes: &mut [u8], group: u32, seed: u32) {
    let checksum = checksum::group_descriptor_checksum(bytes, group, seed).unwrap();
    put_u16(bytes, 30, checksum);
}

fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
