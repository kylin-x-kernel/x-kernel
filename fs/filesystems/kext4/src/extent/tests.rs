// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::{
    BlockMapping, ExtentMappingState,
    mutate::insert_inline_extent_bytes,
    validate::{decode_header, decode_leaf, find_index, map_leaf, validate_extent_entries},
};
use crate::{
    BlockCount, CorruptKind, Ext4Error, PhysicalBlock, UnsupportedKind, disk::extent as disk_extent,
};

#[test]
fn find_index_uses_first_child_before_first_key() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 2, 4, 1);
    put_index(&mut bytes, 0, 100, 20);
    put_index(&mut bytes, 1, 200, 30);

    let header = decode_header(&bytes).expect("decode extent header");
    let selected = find_index(&bytes, header, 50).expect("select child");
    assert_eq!(selected.index.block(), 100);
    assert_eq!(selected.next_lblk, Some(200));
}

#[test]
fn map_leaf_caps_trailing_hole_at_parent_limit() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 1, 4, 0);
    put_leaf(&mut bytes, 0, 100, 10, 1_000);

    let header = decode_header(&bytes).expect("decode extent header");
    assert_eq!(
        map_leaf(&bytes, header, 150, Some(200)),
        Ok(BlockMapping::Hole {
            len: BlockCount::new(50)
        })
    );
}

#[test]
fn validate_extent_entries_rejects_zero_length_extent() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 1, 4, 0);
    put_leaf(&mut bytes, 0, 0, 0, 100);

    let header = decode_header(&bytes).expect("decode extent header");
    assert_eq!(
        validate_extent_entries(&bytes, header, None, None, |_, _| true),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent))
    );
}

#[test]
fn validate_extent_entries_rejects_overlapping_extents() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 2, 4, 0);
    put_leaf(&mut bytes, 0, 10, 5, 100);
    put_leaf(&mut bytes, 1, 14, 5, 200);

    let header = decode_header(&bytes).expect("decode extent header");
    assert_eq!(
        validate_extent_entries(&bytes, header, None, None, |_, _| true),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent))
    );
}

#[test]
fn validate_extent_entries_rejects_physical_system_zone_overlap() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 1, 4, 0);
    put_leaf(&mut bytes, 0, 0, 1, 20);

    let header = decode_header(&bytes).expect("decode extent header");
    assert_eq!(
        validate_extent_entries(&bytes, header, None, None, |block, _| block != 20),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent))
    );
}

#[test]
fn validate_extent_entries_rejects_child_range_overflow() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 1, 4, 0);
    put_leaf(&mut bytes, 0, 100, 20, 1_000);

    let header = decode_header(&bytes).expect("decode extent header");
    assert_eq!(
        validate_extent_entries(&bytes, header, Some(100), Some(110), |_, _| true),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidExtent))
    );
}

#[test]
fn insert_inline_extent_keeps_leaf_order() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 2, 4, 0);
    put_leaf(&mut bytes, 0, 0, 4, 100);
    put_leaf(&mut bytes, 1, 20, 4, 200);

    insert_inline_extent_bytes(
        &mut bytes,
        10,
        PhysicalBlock::new(150),
        BlockCount::new(3),
        ExtentMappingState::Initialized,
        |_, _| true,
    )
    .expect("insert middle inline extent");

    let header = decode_header(&bytes).expect("decode updated header");
    assert_eq!(header.entries(), 3);
    assert_eq!(decode_leaf(&bytes, 0).unwrap().block(), 0);
    assert_eq!(decode_leaf(&bytes, 1).unwrap().block(), 10);
    assert_eq!(decode_leaf(&bytes, 2).unwrap().block(), 20);
    assert_eq!(
        map_leaf(&bytes, header, 10, None),
        Ok(BlockMapping::Mapped {
            physical: PhysicalBlock::new(150),
            len: BlockCount::new(3),
        })
    );
}

#[test]
fn insert_inline_extent_merges_adjacent_initialized_extents() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 2, 4, 0);
    put_leaf(&mut bytes, 0, 0, 4, 100);
    put_leaf(&mut bytes, 1, 8, 4, 108);

    insert_inline_extent_bytes(
        &mut bytes,
        4,
        PhysicalBlock::new(104),
        BlockCount::new(4),
        ExtentMappingState::Initialized,
        |_, _| true,
    )
    .expect("merge inline extents");

    let header = decode_header(&bytes).expect("decode merged header");
    assert_eq!(header.entries(), 1);
    let extent = decode_leaf(&bytes, 0).expect("decode merged extent");
    assert_eq!(extent.block(), 0);
    assert_eq!(extent.start(), PhysicalBlock::new(100));
    assert_eq!(extent.actual_len(), 12);
    assert!(!extent.is_unwritten());
}

#[test]
fn insert_inline_extent_preserves_unwritten_state() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 0, 4, 0);

    insert_inline_extent_bytes(
        &mut bytes,
        0,
        PhysicalBlock::new(100),
        BlockCount::new(7),
        ExtentMappingState::Unwritten,
        |_, _| true,
    )
    .expect("insert unwritten extent");

    let extent = decode_leaf(&bytes, 0).expect("decode unwritten extent");
    assert!(extent.is_unwritten());
    assert_eq!(extent.actual_len(), 7);
}

#[test]
fn insert_inline_extent_splits_run_larger_than_extent_entry() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 0, 4, 0);

    insert_inline_extent_bytes(
        &mut bytes,
        0,
        PhysicalBlock::new(100),
        BlockCount::new(u32::from(disk_extent::EXT_INIT_MAX_LEN) + 2),
        ExtentMappingState::Initialized,
        |_, _| true,
    )
    .expect("split large initialized extent run");

    let header = decode_header(&bytes).expect("decode split header");
    assert_eq!(header.entries(), 2);
    assert_eq!(decode_leaf(&bytes, 0).unwrap().actual_len(), 0x8000);
    assert_eq!(decode_leaf(&bytes, 1).unwrap().block(), 0x8000);
    assert_eq!(decode_leaf(&bytes, 1).unwrap().actual_len(), 2);
    assert_eq!(
        map_leaf(&bytes, header, 0x8000, None),
        Ok(BlockMapping::Mapped {
            physical: PhysicalBlock::new(100 + 0x8000),
            len: BlockCount::new(2),
        })
    );
}

#[test]
fn insert_inline_extent_rejects_overlap() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 1, 4, 0);
    put_leaf(&mut bytes, 0, 10, 5, 100);

    assert_eq!(
        insert_inline_extent_bytes(
            &mut bytes,
            12,
            PhysicalBlock::new(200),
            BlockCount::new(2),
            ExtentMappingState::Initialized,
            |_, _| true,
        ),
        Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation))
    );
}

#[test]
fn insert_inline_extent_rejects_full_inline_root_without_merge() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 1, 1, 0);
    put_leaf(&mut bytes, 0, 0, 2, 100);

    assert_eq!(
        insert_inline_extent_bytes(
            &mut bytes,
            10,
            PhysicalBlock::new(200),
            BlockCount::new(2),
            ExtentMappingState::Initialized,
            |_, _| true,
        ),
        Err(Ext4Error::Unsupported(UnsupportedKind::ExtentMutation))
    );
}

#[test]
fn insert_inline_extent_rejects_indexed_root() {
    let mut bytes = [0; 128];
    put_header(&mut bytes, 1, 4, 1);
    put_index(&mut bytes, 0, 0, 100);

    assert_eq!(
        insert_inline_extent_bytes(
            &mut bytes,
            0,
            PhysicalBlock::new(200),
            BlockCount::new(2),
            ExtentMappingState::Initialized,
            |_, _| true,
        ),
        Err(Ext4Error::Unsupported(UnsupportedKind::ExtentDepth))
    );
}

fn put_header(bytes: &mut [u8], entries: u16, max: u16, depth: u16) {
    put_u16(bytes, 0x00, disk_extent::EXTENT_MAGIC);
    put_u16(bytes, 0x02, entries);
    put_u16(bytes, 0x04, max);
    put_u16(bytes, 0x06, depth);
}

fn put_index(bytes: &mut [u8], entry: usize, block: u32, leaf: u64) {
    let offset = disk_extent::EXTENT_HEADER_SIZE + entry * disk_extent::EXTENT_ENTRY_SIZE;
    put_u32(bytes, offset, block);
    put_u32(bytes, offset + 0x04, leaf as u32);
    put_u16(bytes, offset + 0x08, (leaf >> 32) as u16);
}

fn put_leaf(bytes: &mut [u8], entry: usize, block: u32, len: u16, start: u64) {
    let offset = disk_extent::EXTENT_HEADER_SIZE + entry * disk_extent::EXTENT_ENTRY_SIZE;
    put_u32(bytes, offset, block);
    put_u16(bytes, offset + 0x04, len);
    put_u16(bytes, offset + 0x06, (start >> 32) as u16);
    put_u32(bytes, offset + 0x08, start as u32);
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
