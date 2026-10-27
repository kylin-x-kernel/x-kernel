// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::BTreeMap, sync::Arc};

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{
    BlockCount, BlockGroupNumber, FilesystemBlock, LogicalBlock, PhysicalBlock,
    error::CorruptKind,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    mballoc::Ext4AllocationFlags,
    superblock::test_fixtures::{
        AllocatorGroupSpec, TEST_FREE_BLOCKS, TEST_FREE_INODES,
        allocator_multigroup_checksum_test_filesystem, allocator_multigroup_test_filesystem,
        allocator_test_filesystem, allocator_test_filesystem_with_options,
        allocate_checkpointed_regular_inode, insert_prealloc_space, prealloc_count,
        prealloc_twin_count,
    },
};

// ---- range_end_containing / bit_in_ranges ----

#[def_test]
fn range_end_empty_returns_none() {
    let ranges: &[(u32, u32)] = &[];
    assert!(range_end_containing(ranges, 0).is_none());
    assert!(!bit_in_ranges(ranges, 0));
}

#[def_test]
fn range_end_bit_before_all() {
    let ranges = &[(10, 20), (30, 40)];
    assert!(range_end_containing(ranges, 5).is_none());
    assert!(!bit_in_ranges(ranges, 5));
}

#[def_test]
fn range_end_bit_at_start() {
    let ranges = &[(10, 20)];
    assert_eq!(range_end_containing(ranges, 10), Some(20));
    assert!(bit_in_ranges(ranges, 10));
}

#[def_test]
fn range_end_bit_inside() {
    let ranges = &[(10, 20)];
    assert_eq!(range_end_containing(ranges, 15), Some(20));
    assert!(bit_in_ranges(ranges, 15));
}

#[def_test]
fn range_end_bit_one_before_end() {
    let ranges = &[(10, 20)];
    // [10, 20) means bit 19 is the last covered bit.
    assert_eq!(range_end_containing(ranges, 19), Some(20));
    assert!(bit_in_ranges(ranges, 19));
}

#[def_test]
fn range_end_bit_at_end_boundary() {
    let ranges = &[(10, 20)];
    // End is exclusive: 20 is NOT covered.
    assert!(range_end_containing(ranges, 20).is_none());
    assert!(!bit_in_ranges(ranges, 20));
}

#[def_test]
fn range_end_bit_after_all() {
    let ranges = &[(10, 20), (30, 40)];
    assert!(range_end_containing(ranges, 50).is_none());
    assert!(!bit_in_ranges(ranges, 50));
}

#[def_test]
fn range_end_bit_between_ranges() {
    let ranges = &[(10, 20), (30, 40)];
    // Gap between ranges.
    assert!(range_end_containing(ranges, 25).is_none());
    assert!(!bit_in_ranges(ranges, 25));
}

#[def_test]
fn range_end_bit_in_second_range() {
    let ranges = &[(10, 20), (30, 40)];
    assert_eq!(range_end_containing(ranges, 35), Some(40));
    assert!(bit_in_ranges(ranges, 35));
}

// ---- PreallocSpace::logical_end ----

#[def_test]
fn logical_end_simple() {
    let space = PreallocSpace {
        group: BlockGroupNumber::new(0),
        logical_start: LogicalBlock::new(5),
        phys_start: FilesystemBlock::new(100),
        phys_len: BlockCount::new(3),
    };
    assert_eq!(space.logical_end(), 8);
}

#[def_test]
fn logical_end_zero_length() {
    let space = PreallocSpace {
        group: BlockGroupNumber::new(0),
        logical_start: LogicalBlock::new(5),
        phys_start: FilesystemBlock::new(100),
        phys_len: BlockCount::new(0),
    };
    assert_eq!(space.logical_end(), 5);
}

// ---- reserve_in_inode_map ----

fn make_space(
    logical_start: u64,
    phys_start: u64,
    phys_len: u32,
) -> PreallocSpace {
    PreallocSpace {
        group: BlockGroupNumber::new(0),
        logical_start: LogicalBlock::new(logical_start),
        phys_start: FilesystemBlock::new(phys_start),
        phys_len: BlockCount::new(phys_len),
    }
}

fn space_logical_end(logical_start: u64, phys_len: u32) -> u64 {
    logical_start + u64::from(phys_len)
}

#[def_test]
fn insert_into_empty_map() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    let space = make_space(10, 100, 5);
    let outcome = reserve_in_inode_map(&mut map, space, space_logical_end(10, 5), Some(MAX_PREALLOC_SPACES_PER_INODE));
    assert!(matches!(outcome, PreallocInsertion::Inserted));
    assert_eq!(map.len(), 1);
    assert_eq!(map.get(&LogicalBlock::new(10)).unwrap().phys_len, BlockCount::new(5));
}

#[def_test]
fn rejected_overlapping_predecessor() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    // existing [0, 5) — covers logical 0..4
    map.insert(LogicalBlock::new(0), make_space(0, 100, 5));
    // candidate [3, 8) — overlaps because 3 < 5
    let space = make_space(3, 105, 5);
    let outcome = reserve_in_inode_map(&mut map, space, space_logical_end(3, 5), Some(MAX_PREALLOC_SPACES_PER_INODE));
    assert!(matches!(outcome, PreallocInsertion::Rejected));
    assert_eq!(map.len(), 1); // unchanged
}

#[def_test]
fn rejected_same_key() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    map.insert(LogicalBlock::new(10), make_space(10, 100, 5));
    let space = make_space(10, 200, 5);
    let outcome = reserve_in_inode_map(&mut map, space, space_logical_end(10, 5), Some(MAX_PREALLOC_SPACES_PER_INODE));
    assert!(matches!(outcome, PreallocInsertion::Rejected));
}

#[def_test]
fn rejected_overlapping_successor() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    // existing [10, 15)
    map.insert(LogicalBlock::new(10), make_space(10, 100, 5));
    // candidate [0, 12) — 10 < 12 → successor overlap
    let space = make_space(0, 200, 12);
    let outcome = reserve_in_inode_map(&mut map, space, space_logical_end(0, 12), Some(MAX_PREALLOC_SPACES_PER_INODE));
    assert!(matches!(outcome, PreallocInsertion::Rejected));
    assert_eq!(map.len(), 1);
}

#[def_test]
fn rejected_successor_fully_inside_candidate() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    // existing [5, 8)
    map.insert(LogicalBlock::new(5), make_space(5, 100, 3));
    // candidate [0, 10) covers the successor entirely
    let space = make_space(0, 200, 10);
    let outcome = reserve_in_inode_map(&mut map, space, space_logical_end(0, 10), Some(MAX_PREALLOC_SPACES_PER_INODE));
    assert!(matches!(outcome, PreallocInsertion::Rejected));
}

#[def_test]
fn merged_adjacent_same_group_contiguous() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    // predecessor [0, 5), phys [100, 105)
    map.insert(LogicalBlock::new(0), make_space(0, 100, 5));
    // candidate [5, 8), phys [105, 108) — adjacent both logically and physically
    let space = make_space(5, 105, 3);
    let outcome = reserve_in_inode_map(&mut map, space, space_logical_end(5, 3), Some(MAX_PREALLOC_SPACES_PER_INODE));
    assert!(matches!(outcome, PreallocInsertion::Merged { .. }));
    // Merged entry: [0, 8), phys [100, 108), len=8
    let merged = map.get(&LogicalBlock::new(0)).unwrap();
    assert_eq!(merged.phys_len, BlockCount::new(8));
    assert_eq!(merged.logical_start, LogicalBlock::new(0));
    assert_eq!(merged.phys_start, FilesystemBlock::new(100));
    assert_eq!(map.len(), 1);
}

#[def_test]
fn no_merge_different_group() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    // predecessor in group 0
    map.insert(
        LogicalBlock::new(0),
        PreallocSpace {
            group: BlockGroupNumber::new(0),
            logical_start: LogicalBlock::new(0),
            phys_start: FilesystemBlock::new(100),
            phys_len: BlockCount::new(5),
        },
    );
    // candidate in group 1, logically adjacent, physically contiguous
    let space = PreallocSpace {
        group: BlockGroupNumber::new(1),
        logical_start: LogicalBlock::new(5),
        phys_start: FilesystemBlock::new(105),
        phys_len: BlockCount::new(3),
    };
    let outcome = reserve_in_inode_map(&mut map, space, space_logical_end(5, 3), Some(MAX_PREALLOC_SPACES_PER_INODE));
    // Different groups prevent merge.
    assert!(matches!(outcome, PreallocInsertion::Inserted));
    assert_eq!(map.len(), 2);
}

#[def_test]
fn no_merge_physical_gap() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    // predecessor [0, 5), phys [100, 105)
    map.insert(LogicalBlock::new(0), make_space(0, 100, 5));
    // candidate [5, 8), phys [200, 203) — logically adjacent but physical gap
    let space = make_space(5, 200, 3);
    let outcome = reserve_in_inode_map(&mut map, space, space_logical_end(5, 3), Some(MAX_PREALLOC_SPACES_PER_INODE));
    assert!(matches!(outcome, PreallocInsertion::Inserted));
    assert_eq!(map.len(), 2);
}

#[def_test]
fn no_merge_overflow_sum() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    // Each at u32::MAX, sum would overflow u32.
    map.insert(
        LogicalBlock::new(0),
        make_space(0, 100, u32::MAX),
    );
    let space = make_space(u64::from(u32::MAX), 100 + u64::from(u32::MAX), u32::MAX);
    // They are adjacent logically and physically, but the sum overflows.
    let outcome = reserve_in_inode_map(
        &mut map,
        space,
        space_logical_end(u64::from(u32::MAX), u32::MAX), Some(MAX_PREALLOC_SPACES_PER_INODE)
    );
    // Should be Inserted, not Merged, because the sum overflows u32.
    assert!(matches!(outcome, PreallocInsertion::Inserted));
    assert_eq!(map.len(), 2);
}

#[def_test]
fn insert_multiple_non_overlapping() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    let a = make_space(0, 100, 5);
    let b = make_space(10, 200, 5);
    let c = make_space(5, 300, 5); // between a and b, different phys
    assert!(matches!(
        reserve_in_inode_map(&mut map, a, space_logical_end(0, 5), Some(MAX_PREALLOC_SPACES_PER_INODE)),
        PreallocInsertion::Inserted
    ));
    assert!(matches!(
        reserve_in_inode_map(&mut map, b, space_logical_end(10, 5), Some(MAX_PREALLOC_SPACES_PER_INODE)),
        PreallocInsertion::Inserted
    ));
    assert!(matches!(
        reserve_in_inode_map(&mut map, c, space_logical_end(5, 5), Some(MAX_PREALLOC_SPACES_PER_INODE)),
        PreallocInsertion::Inserted
    ));
    assert_eq!(map.len(), 3);
}

#[def_test]
fn birth_cap_rejects_new_entries_but_not_restores() {
    let mut map: BTreeMap<LogicalBlock, PreallocSpace> = BTreeMap::new();
    for index in 0..MAX_PREALLOC_SPACES_PER_INODE as u64 {
        let logical = index * 10;
        let space = make_space(logical, logical, 5);
        assert!(matches!(
            reserve_in_inode_map(
                &mut map,
                space,
                space_logical_end(logical, 5),
                Some(MAX_PREALLOC_SPACES_PER_INODE),
            ),
            PreallocInsertion::Inserted
        ));
    }
    assert_eq!(map.len(), MAX_PREALLOC_SPACES_PER_INODE);

    let over_cap = make_space(10_000, 10_000, 5);
    assert!(matches!(
        reserve_in_inode_map(
            &mut map,
            over_cap,
            space_logical_end(10_000, 5),
            Some(MAX_PREALLOC_SPACES_PER_INODE),
        ),
        PreallocInsertion::Rejected
    ));
    // An adjacent birth still fuses at the cap instead of growing the map.
    let last = u64::try_from(MAX_PREALLOC_SPACES_PER_INODE - 1).unwrap() * 10;
    let fused = make_space(last + 5, last + 5, 5);
    assert!(matches!(
        reserve_in_inode_map(
            &mut map,
            fused,
            space_logical_end(last + 5, 5),
            Some(MAX_PREALLOC_SPACES_PER_INODE),
        ),
        PreallocInsertion::Merged { .. }
    ));
    assert_eq!(map.len(), MAX_PREALLOC_SPACES_PER_INODE);
    // A restore must always re-insert its vacated span.
    let restored = make_space(20_000, 20_000, 5);
    assert!(matches!(
        reserve_in_inode_map(&mut map, restored, space_logical_end(20_000, 5), None),
        PreallocInsertion::Inserted
    ));
}

// ---- helpers for prealloc lifecycle tests ----

/// Returns preallocated_blocks counter.
fn preallocated_blocks(filesystem: &Ext4SbInfo) -> u64 {
    let alloc = lock(&filesystem.allocator);
    alloc.preallocated_blocks
}

// ---- consume_prealloc_for_write ----

#[def_test]
fn consume_entire_space() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 4, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(0), 4, &mut handle)
        .unwrap();
    drop(handle);

    // Consumed the entire [0, 4) space → no head or tail.
    assert_eq!(result, Some((PhysicalBlock::new(6), BlockCount::new(4))));
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
    assert_eq!(preallocated_blocks(&filesystem), 0);
}

#[def_test]
fn consume_prefix_leaves_tail() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 8, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(0), 3, &mut handle)
        .unwrap();
    drop(handle);

    assert_eq!(result, Some((PhysicalBlock::new(6), BlockCount::new(3))));
    // Tail [3, 8) at phys 9, len 5.
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);
    assert_eq!(preallocated_blocks(&filesystem), 5);

    let space = inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(3)).copied());
    assert_eq!(space.unwrap().phys_start, FilesystemBlock::new(9));
    assert_eq!(space.unwrap().phys_len, BlockCount::new(5));
}

#[def_test]
fn consume_suffix_leaves_head() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 8, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(5), 3, &mut handle)
        .unwrap();
    drop(handle);

    assert_eq!(result, Some((PhysicalBlock::new(11), BlockCount::new(3))));
    // Head [0, 5) at phys 6, len 5.
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);
    assert_eq!(preallocated_blocks(&filesystem), 5);

    let space = inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(0)).copied());
    assert_eq!(space.unwrap().phys_start, FilesystemBlock::new(6));
    assert_eq!(space.unwrap().phys_len, BlockCount::new(5));
}

#[def_test]
fn consume_interior_splits_to_head_and_tail() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 10, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(3), 3, &mut handle)
        .unwrap();
    drop(handle);

    assert_eq!(result, Some((PhysicalBlock::new(9), BlockCount::new(3))));
    // Head [0, 3) at phys 6, len 3; tail [6, 10) at phys 12, len 4.
    assert_eq!(prealloc_count(&inode), 2);
    assert_eq!(prealloc_twin_count(&filesystem), 2);
    assert_eq!(preallocated_blocks(&filesystem), 7);

    let head = inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(0)).copied());
    assert_eq!(head.unwrap().phys_start, FilesystemBlock::new(6));
    assert_eq!(head.unwrap().phys_len, BlockCount::new(3));

    let tail = inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(6)).copied());
    assert_eq!(tail.unwrap().phys_start, FilesystemBlock::new(12));
    assert_eq!(tail.unwrap().phys_len, BlockCount::new(4));
}

#[def_test]
fn consume_empty_request_returns_none() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 4, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(0), 0, &mut handle)
        .unwrap();
    drop(handle);

    assert_eq!(result, None);
    // Space untouched.
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(preallocated_blocks(&filesystem), 4);
}

#[def_test]
fn consume_empty_inode_returns_none() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(0), 4, &mut handle)
        .unwrap();
    drop(handle);

    assert_eq!(result, None);
    assert_eq!(prealloc_count(&inode), 0);
}

#[def_test]
fn consume_no_matching_space_returns_none() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 10, 6, 4, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(0), 4, &mut handle)
        .unwrap();
    drop(handle);

    assert_eq!(result, None);
    assert_eq!(prealloc_count(&inode), 1);
}

#[def_test]
fn consume_overflow_bound_returns_none() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(u64::MAX), 1, &mut handle)
        .unwrap();
    drop(handle);

    assert_eq!(result, None);
}

#[def_test]
fn consume_clips_to_space_end() {
    // Request extends past the space; only the overlap is consumed.
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 5, 6, 4, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(7), 10, &mut handle)
        .unwrap();
    drop(handle);

    // Space covers [5, 9). Request [7, 17) → consumed [7, 9) = 2 blocks.
    assert_eq!(result, Some((PhysicalBlock::new(8), BlockCount::new(2))));
    // Head [5, 7) at phys 6, len 2; no tail.
    assert_eq!(prealloc_count(&inode), 1);
    let head = inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(5)).copied());
    assert_eq!(head.unwrap().phys_len, BlockCount::new(2));
}

#[def_test]
fn consume_then_consume_remainder() {
    // Two consecutive consumes that drain a space in two calls.
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 8, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let r1 = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(0), 3, &mut handle)
        .unwrap();
    let r2 = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(3), 5, &mut handle)
        .unwrap();
    drop(handle);

    assert_eq!(r1, Some((PhysicalBlock::new(6), BlockCount::new(3))));
    assert_eq!(r2, Some((PhysicalBlock::new(9), BlockCount::new(5))));
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(preallocated_blocks(&filesystem), 0);
}

#[def_test]
fn consume_preserves_conservation_invariant() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    // Two disjoint spaces.
    insert_prealloc_space(&filesystem, &inode, 0, 6, 4, 0);
    insert_prealloc_space(&filesystem, &inode, 10, 12, 6, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    // Consume interior of first space → head [0,1), consumed [1,3), tail [3,4).
    let _ = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(1), 2, &mut handle)
        .unwrap();
    drop(handle);

    // preallocated_blocks must equal the sum of twin phys_len.
    let alloc = lock(&filesystem.allocator);
    let sum: u64 = alloc
        .preallocs_by_group
        .values()
        .map(|twin| u64::from(twin.phys_len.get()))
        .sum();
    assert_eq!(alloc.preallocated_blocks, sum);
    // head [0,1) len=1 + tail [3,4) len=1 + second space len=6 = 8
    assert_eq!(alloc.preallocated_blocks, 8);
    drop(alloc);
}

// ---- restore path (credits too low to mark consumed blocks) ----

#[def_test]
fn consume_insufficient_credits_restores_entire_space() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 4, 0);

    // BLOCK_ALLOCATOR_METADATA_CREDITS == 3; 2 credits force
    // ensure_metadata_credits in allocate_block_run_in_group_locked to fail.
    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(0), 4, &mut handle);
    drop(handle);

    // Allocation failed before marking → restore path → Err.
    assert!(result.is_err());
    // Both sides unchanged.
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);
    assert_eq!(preallocated_blocks(&filesystem), 4);
    let space = inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(0)).copied());
    assert_eq!(space.unwrap().phys_len, BlockCount::new(4));
}

#[def_test]
fn consume_insufficient_credits_restores_split_to_head_and_tail() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 8, 0);

    // Interior consume [2, 5) from [0, 8) → head [0,2) + tail [5,8).
    // Low credits force allocation failure; restore must re-merge the span.
    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
    let result = filesystem
        .consume_prealloc_for_write(&inode, LogicalBlock::new(2), 3, &mut handle);
    drop(handle);

    assert!(result.is_err());
    // Restore re-inserts the consumed span [2,5) which merges with head
    // [0,2) → [0,5) len=5 at phys 6.  The tail [5,8) len=3 at phys 11
    // was independently re-stored before the allocation attempt and is
    // untouched by restore, so two entries remain.
    assert_eq!(prealloc_count(&inode), 2);
    assert_eq!(prealloc_twin_count(&filesystem), 2);
    assert_eq!(preallocated_blocks(&filesystem), 8);
    let head = inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(0)).copied());
    assert_eq!(head.unwrap().phys_len, BlockCount::new(5));
    assert_eq!(head.unwrap().phys_start, FilesystemBlock::new(6));
    let tail = inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(5)).copied());
    assert_eq!(tail.unwrap().phys_len, BlockCount::new(3));
    assert_eq!(tail.unwrap().phys_start, FilesystemBlock::new(11));
}

// ---- release_preallocs ----

#[def_test]
fn release_clears_all_spaces() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let arc_inode = Arc::new(inode);
    insert_prealloc_space(&filesystem, &arc_inode, 0, 6, 4, 0);
    insert_prealloc_space(&filesystem, &arc_inode, 10, 12, 6, 0);

    let released = filesystem.release_preallocs(&arc_inode);
    assert_eq!(released, 10);
    assert_eq!(prealloc_count(&arc_inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
    assert_eq!(preallocated_blocks(&filesystem), 0);
}

#[def_test]
fn release_empty_inode_returns_zero() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);

    let released = filesystem.release_preallocs(&inode);
    assert_eq!(released, 0);
}

// ---- clip_preallocs_from_logical ----

#[def_test]
fn clip_straddle_shortens_space() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let arc_inode = Arc::new(inode);
    insert_prealloc_space(&filesystem, &arc_inode, 0, 6, 10, 0);

    // Clip at logical 6: space [0, 10) straddles → keep [0, 6), drop [6, 10).
    let released = filesystem.clip_preallocs_from_logical(&arc_inode, 6);
    assert_eq!(released, 4);
    assert_eq!(prealloc_count(&arc_inode), 1);
    assert_eq!(preallocated_blocks(&filesystem), 6);

    let space = arc_inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(0)).copied());
    assert_eq!(space.unwrap().phys_len, BlockCount::new(6));
    // Group twin also shortened.
    let alloc = lock(&filesystem.allocator);
    let twin = alloc
        .preallocs_by_group
        .get(&(BlockGroupNumber::new(0), FilesystemBlock::new(6)))
        .unwrap();
    assert_eq!(twin.phys_len, BlockCount::new(6));
    drop(alloc);
}

#[def_test]
fn clip_past_all_spaces_does_nothing() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let arc_inode = Arc::new(inode);
    insert_prealloc_space(&filesystem, &arc_inode, 0, 6, 4, 0);

    let released = filesystem.clip_preallocs_from_logical(&arc_inode, 100);
    assert_eq!(released, 0);
    assert_eq!(prealloc_count(&arc_inode), 1);
}

#[def_test]
fn clip_at_zero_drops_all_spaces() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let arc_inode = Arc::new(inode);
    insert_prealloc_space(&filesystem, &arc_inode, 0, 6, 4, 0);
    insert_prealloc_space(&filesystem, &arc_inode, 10, 12, 6, 0);

    let released = filesystem.clip_preallocs_from_logical(&arc_inode, 0);
    assert_eq!(released, 10);
    assert_eq!(prealloc_count(&arc_inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
    assert_eq!(preallocated_blocks(&filesystem), 0);
}

#[def_test]
fn clip_drops_tail_spaces_whole() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let arc_inode = Arc::new(inode);
    insert_prealloc_space(&filesystem, &arc_inode, 0, 6, 5, 0);
    insert_prealloc_space(&filesystem, &arc_inode, 10, 12, 8, 0);

    // Clip at logical 8: no straddler ([0, 5) ends below 8), tail [10, 18) dies whole.
    let released = filesystem.clip_preallocs_from_logical(&arc_inode, 8);
    assert_eq!(released, 8);
    assert_eq!(prealloc_count(&arc_inode), 1);
    assert_eq!(preallocated_blocks(&filesystem), 5);
}

// ---- discard_preallocs_for_space ----

#[def_test]
fn discard_needed_zero_sweeps_all() {
    let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: 22,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: 22,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
    ]);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    // Group 0: space at phys 6 (block 0 is boot, 1=superblock, 2=block bmp, 3=inode bmp, 4-5=inode table; 0x3f → blocks 0-5 used, 6+ free)
    insert_prealloc_space(&filesystem, &inode, 0, 6, 4, 0);
    // Group 1: space at phys 32+6=38 (group 1 starts at block 32)
    insert_prealloc_space(&filesystem, &inode, 10, 38, 6, 1);

    let freed = filesystem.discard_preallocs_for_space(0);
    assert_eq!(freed, 10);
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
    assert_eq!(preallocated_blocks(&filesystem), 0);
}

#[def_test]
fn discard_needed_partial_sweeps_enough_groups() {
    let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: 22,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: 22,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
    ]);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 4, 0); // group 0: 4 blocks
    insert_prealloc_space(&filesystem, &inode, 10, 38, 6, 1); // group 1: 6 blocks

    // Need 5 blocks: group 0 has 4 (< 5), so both groups are doomed.
    let freed = filesystem.discard_preallocs_for_space(5);
    assert_eq!(freed, 10);
    assert_eq!(prealloc_count(&inode), 0);
}

#[def_test]
fn discard_needed_satisfied_by_first_group() {
    let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: 22,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: 22,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
    ]);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, 10, 0); // group 0: 10 blocks
    insert_prealloc_space(&filesystem, &inode, 10, 38, 6, 1); // group 1: 6 blocks

    // Need 3 blocks: group 0 alone has 10 ≥ 3, so only group 0 is doomed.
    let freed = filesystem.discard_preallocs_for_space(3);
    assert_eq!(freed, 10);
    // Group 1 should survive.
    let alloc = lock(&filesystem.allocator);
    assert!(alloc
        .preallocs_by_group
        .contains_key(&(BlockGroupNumber::new(1), FilesystemBlock::new(38))));
    assert!(!alloc
        .preallocs_by_group
        .contains_key(&(BlockGroupNumber::new(0), FilesystemBlock::new(6))));
    drop(alloc);
    // Inode-side: only the group-1 space remains.
    assert_eq!(prealloc_count(&inode), 1);
    let space = inode.prealloc_map().update(|m| m.get(&LogicalBlock::new(10)).copied());
    assert!(space.is_some());
}

#[def_test]
fn discard_no_spaces_returns_zero() {
    let (filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);

    let freed = filesystem.discard_preallocs_for_space(0);
    assert_eq!(freed, 0);
}

#[def_test]
fn discard_cross_inode_cleans_all_owners() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode1 = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    let inode2 = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode1, 0, 6, 4, 0);
    insert_prealloc_space(&filesystem, &inode2, 0, 12, 4, 0);

    let freed = filesystem.discard_preallocs_for_space(0);
    assert_eq!(freed, 8);
    assert_eq!(prealloc_count(&inode1), 0);
    assert_eq!(prealloc_count(&inode2), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
    assert_eq!(preallocated_blocks(&filesystem), 0);
}

#[def_test]
fn discard_needed_zero_skips_groups_without_spaces() {
    let (mut filesystem, _device) = allocator_multigroup_test_filesystem(&[
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: 22,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: 22,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
    ]);
    // Only group 1 has spaces; group 0 has none.
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 10, 38, 6, 1);

    let freed = filesystem.discard_preallocs_for_space(0);
    // needed=0 sweeps all groups that hold spaces; group 0 has none, group 1 has 6.
    assert_eq!(freed, 6);
}

// ---- birth path: allocate_blocks_for_write_with_prealloc ----
//
// The entry is exercised end to end on the standard fixture (group 0 with
// blocks 6..31 free). The sampler's window is structurally single-group
// (`free_run_len_from` measures inside the group's cache) and can never be
// physically adjacent to an existing space (the scan excludes preallocated
// bits and the sample starts at the allocation's end), so the Merged
// accounting goes through `reserve_prealloc_locked` directly. The
// cross-group and physical-overlap rejections are debug-assert depth guards
// that abort in place; the kernel harness has no should_panic, so they stay
// uncovered by design.

/// Runs the birth entry on the standard fixture: a `len`-block run at goal
/// block 6 (leaving the free tail at block 10) with the given logical start
/// and surplus budget.
fn birth_entry(
    filesystem: &Ext4SbInfo,
    inode: &Arc<Ext4Inode>,
    logical_start: u64,
    len: u32,
    budget: u32,
) -> (Ext4AllocatedRun, u32) {
    let request = Ext4AllocationRequest::new(
        LogicalBlock::new(logical_start),
        Some(FilesystemBlock::new(6)),
        BlockCount::new(len),
        BlockCount::new(len),
        Ext4AllocationFlags::EXACT,
        BlockGroupNumber::new(0),
    )
    .unwrap();
    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem
        .allocate_blocks_for_write_with_prealloc(request, inode, budget, &mut handle)
        .unwrap();
    drop(handle);
    result
}

/// Fetches the space at one key from both index sides: the inode-side entry
/// plus the group-side twin's logical start and length (group 0 on the
/// standard fixture).
fn space_both_sides(
    filesystem: &Ext4SbInfo,
    inode: &Ext4Inode,
    logical_start: u64,
    phys_start: u64,
) -> Option<(PreallocSpace, LogicalBlock, BlockCount)> {
    let space = inode
        .prealloc_map()
        .update(|preallocs| preallocs.get(&LogicalBlock::new(logical_start)).copied())?;
    let alloc = lock(&filesystem.allocator);
    let twin = alloc
        .preallocs_by_group
        .get(&(BlockGroupNumber::new(0), FilesystemBlock::new(phys_start)))?;
    Some((space, twin.logical_start, twin.phys_len))
}

/// Builds the group-0 space shape the assertions compare against.
fn group0_space(logical_start: u64, phys_start: u64, phys_len: u32) -> PreallocSpace {
    PreallocSpace {
        group: BlockGroupNumber::new(0),
        logical_start: LogicalBlock::new(logical_start),
        phys_start: FilesystemBlock::new(phys_start),
        phys_len: BlockCount::new(phys_len),
    }
}

#[def_test]
fn birth_budget_clips_surplus() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));

    // The run [6, 10) leaves a 22-block free tail; the budget of 8 clips it.
    let (run, registered) = birth_entry(&filesystem, &inode, 0, 4, 8);

    assert_eq!(run.physical_start(), PhysicalBlock::new(6));
    assert_eq!(run.block_count(), BlockCount::new(4));
    assert_eq!(registered, 8);
    assert_eq!(prealloc_count(&inode), 1);
    let (space, twin_logical, twin_len) = space_both_sides(&filesystem, &inode, 4, 10).unwrap();
    assert_eq!(space, group0_space(4, 10, 8));
    assert_eq!(
        (twin_logical, twin_len),
        (LogicalBlock::new(4), BlockCount::new(8))
    );
    assert_eq!(preallocated_blocks(&filesystem), 8);
}

#[def_test]
fn birth_free_run_clips_surplus() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));

    // The budget outruns the 22-block free tail behind the run [6, 10).
    let (run, registered) = birth_entry(&filesystem, &inode, 0, 4, 100);

    assert_eq!(run.block_count(), BlockCount::new(4));
    assert_eq!(registered, 22);
    assert_eq!(prealloc_count(&inode), 1);
    let (space, twin_logical, twin_len) = space_both_sides(&filesystem, &inode, 4, 10).unwrap();
    assert_eq!(space, group0_space(4, 10, 22));
    assert_eq!(
        (twin_logical, twin_len),
        (LogicalBlock::new(4), BlockCount::new(22))
    );
    // The surplus stays free in the aggregate (Linux parity: statfs shows
    // unconsumed surplus as free).
    assert_eq!(filesystem.free_blocks_count(), 22);
    assert_eq!(preallocated_blocks(&filesystem), 22);
}

#[def_test]
fn birth_space_clips_surplus() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    // An existing space at [18, 24) caps the sample behind the run [6, 10):
    // the cache does not see spaces, the index query clips to 18 - 10 = 8.
    insert_prealloc_space(&filesystem, &inode, 100, 18, 6, 0);

    let (_run, registered) = birth_entry(&filesystem, &inode, 0, 4, 100);

    assert_eq!(registered, 8);
    assert_eq!(prealloc_count(&inode), 2);
    let (space, twin_logical, twin_len) = space_both_sides(&filesystem, &inode, 4, 10).unwrap();
    assert_eq!(space, group0_space(4, 10, 8));
    assert_eq!(
        (twin_logical, twin_len),
        (LogicalBlock::new(4), BlockCount::new(8))
    );
    let (space, twin_logical, twin_len) = space_both_sides(&filesystem, &inode, 100, 18).unwrap();
    assert_eq!(space, group0_space(100, 18, 6));
    assert_eq!(
        (twin_logical, twin_len),
        (LogicalBlock::new(100), BlockCount::new(6))
    );
    assert_eq!(preallocated_blocks(&filesystem), 14);
}

#[def_test]
fn birth_zero_budget_registers_nothing() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));

    let (run, registered) = birth_entry(&filesystem, &inode, 0, 4, 0);

    assert_eq!(run.block_count(), BlockCount::new(4));
    assert_eq!(registered, 0);
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
    assert_eq!(preallocated_blocks(&filesystem), 0);
}

#[def_test]
fn reserve_merged_books_only_the_new_length() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    // Predecessor: logical [0, 6), physical [10, 16). The new window abuts it
    // on both keys, so the pair fuses into logical [0, 10) / physical
    // [10, 20).
    insert_prealloc_space(&filesystem, &inode, 0, 10, 6, 0);

    let accepted = {
        let mut alloc = lock(&filesystem.allocator);
        filesystem.reserve_prealloc_locked(
            &mut alloc,
            &inode,
            LogicalBlock::new(6),
            FilesystemBlock::new(16),
            BlockCount::new(4),
        )
    };

    assert_eq!(accepted, 4);
    // The inode side keeps one fused entry and the group side one twin at the
    // summed length, while the counter grew by the new window only (6 + 4).
    assert_eq!(prealloc_count(&inode), 1);
    let (space, twin_logical, twin_len) = space_both_sides(&filesystem, &inode, 0, 10).unwrap();
    assert_eq!(space, group0_space(0, 10, 10));
    assert_eq!(
        (twin_logical, twin_len),
        (LogicalBlock::new(0), BlockCount::new(10))
    );
    assert_eq!(prealloc_twin_count(&filesystem), 1);
    assert_eq!(preallocated_blocks(&filesystem), 10);
}

// ---- ENOSPC retry valve ----
//
// A prealloc space covers physical blocks the bitmap still shows free, so a
// scan can come up empty while the free aggregate is non-zero. The valve
// drops whole groups of spaces and rescans; the tests below pin the retry,
// the corrupt no-retry, and the "zero discard ends the loop" behaviours on
// both allocation entry points.

/// Builds a file-data request for `len` blocks at goal block 6 of group 0.
fn group0_request(logical_start: u64, len: u32) -> Ext4AllocationRequest {
    Ext4AllocationRequest::new(
        LogicalBlock::new(logical_start),
        Some(FilesystemBlock::new(6)),
        BlockCount::new(len),
        BlockCount::new(len),
        Ext4AllocationFlags::EXACT,
        BlockGroupNumber::new(0),
    )
    .unwrap()
}

/// Runs the mballoc scan entry with a no-op post-allocation hook.
fn scan_without_birth(
    filesystem: &Ext4SbInfo,
    request: Ext4AllocationRequest,
) -> Ext4Result<Ext4AllocatedRun> {
    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem.allocate_blocks_for_write_with(request, &mut handle, |_, _| {});
    drop(handle);
    result
}

#[def_test]
fn enospc_valve_discards_spaces_and_retries_scan() {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    // One space covers the whole free region [6, 32): the first scan finds no
    // allocatable bit, yet the free aggregate still counts every block.
    insert_prealloc_space(&filesystem, &inode, 0, 6, TEST_FREE_BLOCKS, 0);
    assert_eq!(filesystem.free_blocks_count(), u64::from(TEST_FREE_BLOCKS));

    let run = scan_without_birth(&filesystem, group0_request(0, 4)).unwrap();

    assert_eq!(run.physical_start(), PhysicalBlock::new(6));
    assert_eq!(run.block_count(), BlockCount::new(4));
    // The valve dropped the blocking space from both index sides.
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
    assert_eq!(preallocated_blocks(&filesystem), 0);
}

#[def_test]
fn enospc_valve_without_spaces_passes_nospace_through() {
    // No prealloc space exists, so the valve has nothing to discard: a
    // request the group cannot satisfy must fail with NoSpace, not spin.
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    let free_before = filesystem.free_blocks_count();

    // 27 blocks exceed the group's 26 free blocks; the group is skipped
    // outright (expected_len clamps below min_len), leaving the scan empty.
    let request = Ext4AllocationRequest::new(
        LogicalBlock::new(0),
        Some(FilesystemBlock::new(6)),
        BlockCount::new(TEST_FREE_BLOCKS + 1),
        BlockCount::new(TEST_FREE_BLOCKS + 1),
        Ext4AllocationFlags::EXACT,
        BlockGroupNumber::new(0),
    )
    .unwrap();
    let result = scan_without_birth(&filesystem, request);

    assert_eq!(result, Err(Ext4Error::NoSpace));
    assert_eq!(filesystem.free_blocks_count(), free_before);
    assert_eq!(prealloc_count(&inode), 0);
}

#[def_test]
fn enospc_valve_skips_retry_when_failure_is_geometric() {
    // The request exceeds the group's capacity, so geometry skips the
    // group entirely. No healthy group was attempted, so the valve has
    // nothing to retry — discard cannot change group sizes.
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, TEST_FREE_BLOCKS, 0);

    let request = Ext4AllocationRequest::new(
        LogicalBlock::new(0),
        Some(FilesystemBlock::new(6)),
        BlockCount::new(TEST_FREE_BLOCKS + 4),
        BlockCount::new(TEST_FREE_BLOCKS + 4),
        Ext4AllocationFlags::EXACT,
        BlockGroupNumber::new(0),
    )
    .unwrap();
    let result = scan_without_birth(&filesystem, request);

    assert_eq!(result, Err(Ext4Error::NoSpace));
    // No retry happened: geometry cannot be fixed by discarding.
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);
    assert_eq!(preallocated_blocks(&filesystem), u64::from(TEST_FREE_BLOCKS));
}

#[def_test]
fn enospc_valve_does_not_retry_corrupt_bitmap() {
    // A bitmap-checksum mismatch surfaces as InvalidBlockBitmap. Discard
    // cannot repair corruption, so the valve must stay put: the original
    // error is returned and the space is left intact.
    let (mut filesystem, _device) = allocator_test_filesystem_with_options(
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        true,
        true,
        false,
    );
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, TEST_FREE_BLOCKS, 0);

    let result = scan_without_birth(&filesystem, group0_request(0, 4));

    assert_eq!(result, Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockBitmap)));
    // No discard happened: the corruption suppression held.
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);
}

#[def_test]
fn enospc_valve_retries_healthy_group_despite_corrupt_sibling() {
    // Distinguishes "corruption is only latched" from "corruption closes the
    // retry gate". Group 0's bitmap checksum is bad; group 1's free bits are
    // all held by a prealloc space. The scan reports Corrupt for group 0 and
    // NoSpace for group 1, so the valve must discard group 1's space and serve
    // the request from the healthy group on the rescan.
    let (mut filesystem, _device) = allocator_multigroup_checksum_test_filesystem(
        &[
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0xff, 0x03, 0, 0],
            },
            AllocatorGroupSpec {
                free_blocks: TEST_FREE_BLOCKS,
                free_inodes: TEST_FREE_INODES,
                used_directories: 0,
                flags: 0,
                block_bitmap: [0b0011_1111, 0, 0, 0],
                inode_bitmap: [0xff, 0x03, 0, 0],
            },
        ],
        &[0],
    );
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    // Group 1 spans blocks 32..64, so its first free block (bit 6) is 38.
    insert_prealloc_space(&filesystem, &inode, 0, 38, TEST_FREE_BLOCKS, 1);

    let run = scan_without_birth(&filesystem, group0_request(0, 4)).unwrap();

    assert_eq!(run.group(), BlockGroupNumber::new(1));
    assert_eq!(run.physical_start(), PhysicalBlock::new(38));
    assert_eq!(run.block_count(), BlockCount::new(4));
    // The valve dropped group 1's space; group 0 was never allocatable.
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
    assert_eq!(preallocated_blocks(&filesystem), 0);
}

#[def_test]
fn group_allocation_fails_when_prealloc_spaces_block_the_group() {
    // The single-group entry does not retry with a discard: the discard is
    // group-unaware (it drops groups in ascending order), so it cannot
    // guarantee reaching the target group. The mballoc scan entry handles
    // the ENOSPC retry valve for multi-group scans.
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    insert_prealloc_space(&filesystem, &inode, 0, 6, TEST_FREE_BLOCKS, 0);

    let journal = JournalTransactions::new(TransactionId::new(200));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let result = filesystem.allocate_block_run_in_group(
        BlockGroupNumber::new(0),
        Some(FilesystemBlock::new(6)),
        BlockCount::new(4),
        BlockCount::new(4),
        &mut handle,
    );
    drop(handle);

    assert_eq!(result, Err(Ext4Error::NoSpace));
    // No discard happened: the space is intact.
    assert_eq!(prealloc_count(&inode), 1);
    assert_eq!(prealloc_twin_count(&filesystem), 1);
    assert_eq!(preallocated_blocks(&filesystem), u64::from(TEST_FREE_BLOCKS));
}

#[def_test]
fn discard_preallocs_returns_zero_when_no_spaces_exist() {
    // The valve's termination condition: when `discard` returns 0,
    // nothing was freed, a retry cannot do better, and the loop must
    // end. Exercising this through the full scan entry requires a
    // fragmented bitmap so the scanner hits NoSpace with zero prealloc
    // spaces (a feasible construction via AllocatorGroupSpec, deferred
    // to a follow-up); this test covers the function directly.
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let _inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));

    let freed = filesystem.discard_preallocs_for_space(100);
    assert_eq!(freed, 0);
}

#[def_test]
fn birth_does_not_promise_uncommitted_freed_surplus() {
    let (mut filesystem, _) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    let journal = JournalTransactions::new(TransactionId::new(300));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let freed = filesystem
        .allocate_block(Some(FilesystemBlock::new(10)), &mut handle)
        .unwrap();
    filesystem
        .release_allocated_block(freed.block(), &mut handle)
        .unwrap();
    let request = Ext4AllocationRequest::new(
        LogicalBlock::new(0),
        Some(FilesystemBlock::new(6)),
        BlockCount::new(4),
        BlockCount::new(4),
        Ext4AllocationFlags::EXACT,
        BlockGroupNumber::new(0),
    )
    .unwrap();
    let (run, registered) = filesystem
        .allocate_blocks_for_write_with_prealloc(request, &inode, 8, &mut handle)
        .unwrap();
    assert_eq!(run.physical_start(), PhysicalBlock::new(6));
    assert_eq!(registered, 0);
    assert_eq!(prealloc_count(&inode), 0);
    assert_eq!(prealloc_twin_count(&filesystem), 0);
}
