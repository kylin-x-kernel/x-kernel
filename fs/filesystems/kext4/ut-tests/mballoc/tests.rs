// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;
use crate::bitmap_allocator::BlockGroupRange;

#[def_test]
fn free_extent_cache_suggests_goal_inside_free_run() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(100), 32).unwrap();
    let bitmap = [0xff, 0x0f, 0, 0];
    let cache = BlockGroupFreeExtentCache::from_bitmap(range, &bitmap, |_| false).unwrap();

    assert_eq!(
        cache
            .suggest_goal(
                range,
                Some(FilesystemBlock::new(117)),
                BlockCount::new(1),
                BlockCount::new(8),
            )
            .unwrap(),
        Some(FilesystemBlock::new(117))
    );
}

#[def_test]
fn free_extent_cache_prefers_expected_run_over_short_goal_tail() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(100), 32).unwrap();
    let bitmap = [0xff, 0x7f, 0x0f, 0xf0];
    let cache = BlockGroupFreeExtentCache::from_bitmap(range, &bitmap, |_| false).unwrap();

    assert_eq!(
        cache
            .suggest_goal(
                range,
                Some(FilesystemBlock::new(115)),
                BlockCount::new(1),
                BlockCount::new(8),
            )
            .unwrap(),
        Some(FilesystemBlock::new(120))
    );
}

#[def_test]
fn free_extent_cache_tracks_allocate_and_release() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(100), 16).unwrap();
    let bitmap = [0, 0];
    let mut cache = BlockGroupFreeExtentCache::from_bitmap(range, &bitmap, |_| false).unwrap();

    cache.mark_allocated(4, BlockCount::new(4)).unwrap();
    assert_eq!(cache.free_blocks(), 12);
    assert_eq!(
        cache
            .suggest_goal(
                range,
                Some(FilesystemBlock::new(104)),
                BlockCount::new(1),
                BlockCount::new(4),
            )
            .unwrap(),
        Some(FilesystemBlock::new(100))
    );

    for bit in 4..8 {
        cache.mark_free(bit).unwrap();
    }
    assert_eq!(cache.free_blocks(), 16);
    assert_eq!(
        cache
            .suggest_goal(
                range,
                Some(FilesystemBlock::new(104)),
                BlockCount::new(4),
                BlockCount::new(8),
            )
            .unwrap(),
        Some(FilesystemBlock::new(104))
    );
}

#[def_test]
fn free_extent_cache_uses_order_buckets_for_best_extent() {
    let range =
        BlockGroupRange::new(BlockGroupNumber::new(0), FilesystemBlock::new(200), 64).unwrap();
    let mut bitmap = [0xff; 8];
    bitmap[1] = 0b1111_0000;
    bitmap[4] = 0;
    bitmap[5] = 0;
    let cache = BlockGroupFreeExtentCache::from_bitmap(range, &bitmap, |_| false).unwrap();

    assert_eq!(
        cache
            .suggest_goal(range, None, BlockCount::new(8), BlockCount::new(16),)
            .unwrap(),
        Some(FilesystemBlock::new(232))
    );
}
