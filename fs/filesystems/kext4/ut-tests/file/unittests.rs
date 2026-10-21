// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::*;
use crate::{PhysicalBlock, extent::BlockMappingFlags};

#[def_test]
fn hole_writeback_credits_grow_by_actual_allocation_run() {
    let hole = BlockMapping::Hole {
        len: BlockCount::new(8),
        flags: BlockMappingFlags::empty(),
    };
    let first = OrderedWritebackCreditUsage::default().with_mapping_operation(hole);
    assert_eq!(first.and_then(|usage| usage.credit_bound()), Ok(75));
    assert_eq!(
        first
            .and_then(|usage| usage.with_mapping_operation(hole))
            .and_then(|usage| usage.credit_bound()),
        Ok(142)
    );
}

#[def_test]
fn fragmented_unwritten_credits_follow_actual_conversion_count() {
    let mut usage = OrderedWritebackCreditUsage::default();
    for logical in 0..8u64 {
        let mapping = if logical % 2 == 0 {
            BlockMapping::Mapped {
                physical: PhysicalBlock::new(100 + logical),
                len: BlockCount::new(1),
                flags: BlockMappingFlags::empty(),
            }
        } else {
            BlockMapping::Unwritten {
                physical: PhysicalBlock::new(100 + logical),
                len: BlockCount::new(1),
                flags: BlockMappingFlags::empty(),
            }
        };
        usage = usage.with_mapping_operation(mapping).unwrap();
    }
    assert_eq!(usage.credit_bound(), Ok(136));
}

#[def_test]
fn writeback_cursor_carries_preallocation_budget_across_transactions() {
    let mut cursor = OrderedWritebackCursor::new(12);
    assert_eq!(cursor.advance(4096), Ok(()));
    cursor.remaining_extra_prealloc_blocks -= 3;
    cursor.settle();
    assert_eq!(cursor.input_offset, 4096);
    assert_eq!(cursor.settled_input_offset, 4096);
    assert_eq!(cursor.remaining_extra_prealloc_blocks, 9);
}

#[def_test]
fn mapped_writeback_does_not_grow_metadata_operation_credits() {
    let mapped = BlockMapping::Mapped {
        physical: PhysicalBlock::new(100),
        len: BlockCount::new(4),
        flags: BlockMappingFlags::empty(),
    };
    assert_eq!(
        OrderedWritebackCreditUsage::default()
            .with_mapping_operation(mapped)
            .map(|usage| (usage.allocation_runs, usage.extent_mutations)),
        Ok((0, 0))
    );
}
