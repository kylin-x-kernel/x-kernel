// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cache demand, bounded second chances and maintenance fast-path regressions.

use ktime_types::TimeSpan;
use unittest::def_test;

use super::*;
use crate::buffer::tests::{TEST_BLOCK_SIZE, TestDevice, cache_for};

fn assert_idle_maintenance_skips_index_lock(cache: &MetadataBlockCache) {
    assert!(!cache.inner.has_reclaim_demand.load(Ordering::Relaxed));
    let _index = lock(&cache.inner.entries);
    // A clone observes the same demand hint. Taking the mutex on this path
    // would deadlock, so the assertion exercises the actual maintenance entry.
    assert_eq!(cache.clone().reclaim_for_pressure(64), 0);
}

#[def_test]
fn idle_cache_maintenance_skips_index_lock() {
    let cache = cache_for(Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO)));
    assert_idle_maintenance_skips_index_lock(&cache);
    drop(cache.read(FilesystemBlock::new(1)).unwrap());
    assert_idle_maintenance_skips_index_lock(&cache);
}

#[def_test]
fn cache_demand_rearms_after_reclaim_and_invalidation() {
    let retained = METADATA_CACHE_RETAINED_BYTES / TEST_BLOCK_SIZE;
    let cache = cache_for(Arc::new(TestDevice::new(retained + 1, 0, TimeSpan::ZERO)));
    for block in 0..retained {
        drop(cache.read(FilesystemBlock::new(block as u64)).unwrap());
    }
    assert_idle_maintenance_skips_index_lock(&cache);
    let extra = FilesystemBlock::new(retained as u64);
    drop(cache.read(extra).unwrap());
    assert!(cache.inner.has_reclaim_demand.load(Ordering::Relaxed));
    {
        let _index = lock(&cache.inner.entries);
        assert_eq!(cache.reclaim_for_pressure(0), 0);
    }
    assert_eq!(cache.reclaim_blocks(&[extra]), 1);
    assert_idle_maintenance_skips_index_lock(&cache);

    let transaction = TransactionId::new(19);
    let access = cache.create_access(extra, transaction).unwrap();
    assert_eq!(cache.reclaim_blocks(&[extra]), 0);
    drop(access);
    assert!(cache.inner.has_reclaim_demand.load(Ordering::Relaxed));
    cache
        .forget_or_revoke_checkpointed(extra, transaction)
        .unwrap();
    assert_idle_maintenance_skips_index_lock(&cache);

    drop(cache.read(extra).unwrap());
    assert_eq!(cache.reclaim_unused(usize::MAX), retained + 1);
    assert_idle_maintenance_skips_index_lock(&cache);
    for block in 0..=retained {
        drop(cache.read(FilesystemBlock::new(block as u64)).unwrap());
    }
    assert_eq!(cache.reclaim_for_pressure(1), 1);
    assert_idle_maintenance_skips_index_lock(&cache);
    drop(cache.read(FilesystemBlock::new(0)).unwrap());
    assert!(cache.inner.has_reclaim_demand.load(Ordering::Relaxed));
    cache.invalidate_all();
    assert_idle_maintenance_skips_index_lock(&cache);
    // Invalidation must not prevent a later growing cache from requesting work.
    for block in 0..=retained {
        drop(cache.read(FilesystemBlock::new(block as u64)).unwrap());
    }
    assert_eq!(cache.reclaim_for_pressure(1), 1);
}

#[def_test]
fn failed_cache_load_removes_its_reclaim_demand() {
    let retained = METADATA_CACHE_RETAINED_BYTES / TEST_BLOCK_SIZE;
    let cache = cache_for(Arc::new(TestDevice::new(retained + 1, 0, TimeSpan::ZERO)));
    for block in 0..retained {
        drop(cache.read(FilesystemBlock::new(block as u64)).unwrap());
    }
    // The missing slot briefly crosses the allowance before device bounds
    // validation fails. Removing that slot must also withdraw its demand.
    assert!(
        cache
            .read(FilesystemBlock::new((retained + 1) as u64))
            .is_err()
    );
    assert_eq!(cache.cached_block_count(), retained);
    assert_idle_maintenance_skips_index_lock(&cache);
    drop(cache.read(FilesystemBlock::new(retained as u64)).unwrap());
    assert_eq!(cache.reclaim_for_pressure(1), 1);
    assert_idle_maintenance_skips_index_lock(&cache);
}

#[def_test]
fn incremental_reclaim_gives_reused_blocks_one_bounded_second_chance() {
    let cache = cache_for(Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO)));
    for block in 1..4 {
        drop(cache.read(FilesystemBlock::new(block)).unwrap());
    }
    // Physical key 1 was reused most recently. Inspecting it consumes the whole
    // budget without evicting it or scanning ahead to find a different victim.
    drop(cache.read(FilesystemBlock::new(1)).unwrap());
    assert_eq!(cache.reclaim_unused_incremental(1), 0);
    assert_eq!(cache.cached_block_count(), 3);
    assert_eq!(cache.reclaim_unused_incremental(1), 1);
    assert_eq!(cache.reclaim_unused_incremental(1), 1);
    // Once its chance has been consumed, no further hit protects the block.
    assert_eq!(cache.reclaim_unused_incremental(1), 1);
    assert_eq!(cache.cached_block_count(), 0);
}

#[def_test]
fn explicit_reclaim_does_not_wait_for_recent_access_bits() {
    let cache = cache_for(Arc::new(TestDevice::new(3, 0, TimeSpan::ZERO)));
    for block in 1..3 {
        drop(cache.read(FilesystemBlock::new(block)).unwrap());
        drop(cache.read(FilesystemBlock::new(block)).unwrap());
    }
    assert_eq!(cache.reclaim_blocks(&[FilesystemBlock::new(1)]), 1);
    assert_eq!(cache.reclaim_unused(usize::MAX), 1);
    assert_idle_maintenance_skips_index_lock(&cache);
}

#[def_test]
fn old_snapshot_pins_slot_identity_after_replacement_and_forget() {
    let device = Arc::new(TestDevice::new(4, 0, TimeSpan::ZERO));
    let cache = cache_for(device.clone());
    let block = FilesystemBlock::new(1);
    let old = cache.read(block).unwrap();
    let transaction = TransactionId::new(20);
    let access = cache.write_access(block, transaction).unwrap();
    access
        .update_bytes(|_, bytes| {
            bytes[0] = 0x55;
            Ok(())
        })
        .unwrap();
    drop(access);
    cache
        .forget_or_revoke_checkpointed(block, transaction)
        .unwrap();
    assert_eq!(cache.reclaim_unused(usize::MAX), 0);
    let current = cache.read(block).unwrap();
    assert_eq!(current.as_ref()[0], 0x55);
    assert_eq!(device.read_count.load(Ordering::Relaxed), 1);
    drop(current);
    drop(old);
    assert_eq!(cache.reclaim_unused(usize::MAX), 1);
}
