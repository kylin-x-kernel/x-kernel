// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn allocation_uses_goal_when_available() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(2), FilesystemBlock::new(100), 32).unwrap();
    let mut bitmap = [0xff, 0x0f, 0, 0];

    let allocation = allocate_block_run_from_bitmap(
        &mut bitmap,
        range,
        Some(FilesystemBlock::new(117)),
        BlockCount::new(1),
        BlockCount::new(1),
        &[],
        |_| false,
    )
    .unwrap()
    .first_block_allocation();

    assert_eq!(allocation.group(), BlockGroupNumber::new(2));
    assert_eq!(allocation.block(), PhysicalBlock::new(117));
    assert_eq!(allocation.bitmap_bit(), 17);
    assert_eq!(bitmap[2] & 0b0000_0010, 0b0000_0010);
}

#[def_test]
fn allocation_skips_protected_blocks_and_wraps() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 8).unwrap();
    let mut bitmap = [0b1100_1111];

    let allocation = allocate_block_run_from_bitmap(
        &mut bitmap,
        range,
        Some(FilesystemBlock::new(15)),
        BlockCount::new(1),
        BlockCount::new(1),
        &[],
        |block| block == FilesystemBlock::new(14),
    )
    .unwrap()
    .first_block_allocation();

    assert_eq!(allocation.block(), PhysicalBlock::new(15));
    assert_eq!(bitmap, [0b1110_1111]);
}

#[def_test]
fn allocation_reports_no_space_without_mutating_bitmap() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 4).unwrap();
    let mut bitmap = [0b0000_1110];
    let before = bitmap;

    assert_eq!(
        allocate_block_run_from_bitmap(
            &mut bitmap,
            range,
            None,
            BlockCount::new(1),
            BlockCount::new(1),
            &[],
            |block| { block == FilesystemBlock::new(10) },
        ),
        Err(Ext4Error::NoSpace)
    );
    assert_eq!(bitmap, before);
}

#[def_test]
fn run_allocation_uses_goal_and_marks_contiguous_bits() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 16).unwrap();
    let mut bitmap = [0b1111_1111, 0];

    let allocation = allocate_block_run_from_bitmap(
        &mut bitmap,
        range,
        Some(FilesystemBlock::new(18)),
        BlockCount::new(4),
        BlockCount::new(4),
        &[],
        |_| false,
    )
    .unwrap();

    assert_eq!(allocation.group(), BlockGroupNumber::new(0));
    assert_eq!(allocation.first_block(), PhysicalBlock::new(18));
    assert_eq!(allocation.block_count(), BlockCount::new(4));
    assert_eq!(allocation.first_bitmap_bit(), 8);
    assert_eq!(bitmap, [0xff, 0b0000_1111]);
}

#[def_test]
fn run_allocation_can_return_partial_run_above_minimum() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 16).unwrap();
    let mut bitmap = [0b1111_1111, 0b1111_1100];

    let allocation = allocate_block_run_from_bitmap(
        &mut bitmap,
        range,
        Some(FilesystemBlock::new(18)),
        BlockCount::new(2),
        BlockCount::new(4),
        &[],
        |_| false,
    )
    .unwrap();

    assert_eq!(allocation.first_block(), PhysicalBlock::new(18));
    assert_eq!(allocation.block_count(), BlockCount::new(2));
    assert_eq!(bitmap, [0xff, 0xff]);
}

#[def_test]
fn run_allocation_rejects_runs_shorter_than_minimum_without_mutating_bitmap() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 16).unwrap();
    let mut bitmap = [0b1111_1111, 0b1111_1100];
    let before = bitmap;

    assert_eq!(
        allocate_block_run_from_bitmap(
            &mut bitmap,
            range,
            Some(FilesystemBlock::new(18)),
            BlockCount::new(3),
            BlockCount::new(4),
            &[],
            |_| false,
        ),
        Err(Ext4Error::NoSpace)
    );
    assert_eq!(bitmap, before);
}

#[def_test]
fn run_allocation_stops_at_protected_blocks() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(10), 16).unwrap();
    let mut bitmap = [0b0011_1111, 0xff];

    let allocation = allocate_block_run_from_bitmap(
        &mut bitmap,
        range,
        Some(FilesystemBlock::new(16)),
        BlockCount::new(2),
        BlockCount::new(4),
        &[],
        |block| block == FilesystemBlock::new(18),
    )
    .unwrap();

    assert_eq!(allocation.first_block(), PhysicalBlock::new(16));
    assert_eq!(allocation.block_count(), BlockCount::new(2));
    assert_eq!(bitmap, [0xff, 0xff]);
}

#[def_test]
fn release_clears_allocated_block_and_rejects_duplicate_release() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(50), 8).unwrap();
    let mut bitmap = [0b0000_0100];

    let released =
        release_block_to_bitmap(&mut bitmap, range, FilesystemBlock::new(52), |_| false).unwrap();

    assert_eq!(released.block(), PhysicalBlock::new(52));
    assert_eq!(bitmap, [0]);
    assert_eq!(
        release_block_to_bitmap(&mut bitmap, range, FilesystemBlock::new(52), |_| false),
        Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap))
    );
}

#[def_test]
fn run_allocation_skips_large_excluded_ranges_before_system_zone_checks() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(0), 32768).unwrap();
    let mut bitmap = alloc::vec![0; 4096];
    bitmap[0] = 1;
    let mut probes = 0;
    let allocation = allocate_block_run_from_bitmap(
        &mut bitmap,
        range,
        None,
        BlockCount::new(1),
        BlockCount::new(1),
        &[(1, 32767)],
        |_| {
            probes += 1;
            false
        },
    )
    .unwrap();
    assert_eq!(allocation.first_bitmap_bit(), 32767);
    assert_eq!(probes, 2);
    assert_eq!(bitmap[1], 0);
}

#[def_test]
fn excluded_ranges_stop_runs_and_preserve_wrapped_search() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(100), 32).unwrap();
    let mut bitmap = [0; 4];
    let allocation = allocate_block_run_from_bitmap(
        &mut bitmap,
        range,
        Some(FilesystemBlock::new(125)),
        BlockCount::new(2),
        BlockCount::new(4),
        &[(2, 32)],
        |_| false,
    )
    .unwrap();
    assert_eq!(allocation.first_bitmap_bit(), 0);
    assert_eq!(allocation.block_count(), BlockCount::new(2));
    assert_eq!(bitmap, [3, 0, 0, 0]);
    let before = bitmap;
    assert_eq!(
        allocate_block_run_from_bitmap(
            &mut bitmap,
            range,
            None,
            BlockCount::new(1),
            BlockCount::new(1),
            &[(2, 32)],
            |_| false,
        ),
        Err(Ext4Error::NoSpace)
    );
    assert_eq!(bitmap, before);
}
