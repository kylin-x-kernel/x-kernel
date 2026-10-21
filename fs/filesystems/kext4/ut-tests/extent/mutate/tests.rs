// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{
    MutableExtent, balanced_partition_ranges, extent_tree_metadata_block_count,
    independent_block_extents, insert_extent_run, ordered_writeback_credit_bound,
};
use crate::{BlockCount, PhysicalBlock, extent::ExtentMappingState};

#[def_test]
fn single_block_writeback_covers_maximum_extent_path() {
    assert_eq!(ordered_writeback_credit_bound(1, 2), Ok(75));
}

#[def_test]
fn multi_block_writeback_scales_credit_budget() {
    let single = ordered_writeback_credit_bound(1, 2).unwrap();
    let multiple = ordered_writeback_credit_bound(8, 16).unwrap();

    assert!(multiple > single);
}

#[def_test]
fn writeback_credit_budget_is_not_truncated() {
    assert!(ordered_writeback_credit_bound(8, 16).unwrap() > 512);
}

#[def_test]
fn writeback_credit_budget_follows_mapping_work() {
    assert_eq!(ordered_writeback_credit_bound(0, 0), Ok(8));
    assert_eq!(ordered_writeback_credit_bound(0, 1), Ok(40));
}

#[def_test]
fn local_split_balances_new_partitions() {
    assert_eq!(
        balanced_partition_ranges(341, 340),
        Ok(alloc::vec![0..171, 171..341])
    );
}

#[def_test]
fn independent_block_preflight_preserves_separate_extent_entries() {
    let existing = alloc::vec![
        MutableExtent::new(
            0,
            PhysicalBlock::new(10),
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            |_, _| true,
        )
        .unwrap(),
        MutableExtent::new(
            1,
            PhysicalBlock::new(20),
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            |_, _| true,
        )
        .unwrap(),
        MutableExtent::new(
            2,
            PhysicalBlock::new(30),
            BlockCount::new(1),
            ExtentMappingState::Initialized,
            |_, _| true,
        )
        .unwrap(),
    ];

    let mut contiguous = existing.clone();
    let contiguous_insert = MutableExtent::from_run(
        3,
        PhysicalBlock::new(100),
        BlockCount::new(2),
        ExtentMappingState::Initialized,
        |_, _| true,
    )
    .unwrap();
    insert_extent_run(&mut contiguous, &contiguous_insert).unwrap();

    let mut independent = existing;
    let independent_insert = independent_block_extents(3, BlockCount::new(2)).unwrap();
    insert_extent_run(&mut independent, &independent_insert).unwrap();

    assert_eq!(contiguous.len(), 4);
    assert_eq!(independent.len(), 5);
    assert_eq!(
        extent_tree_metadata_block_count(contiguous.len(), 4096),
        Ok(0)
    );
    assert_eq!(
        extent_tree_metadata_block_count(independent.len(), 4096),
        Ok(1)
    );
}
