// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::LruCache;

#[def_test]
fn test_lru_put_evict() {
    let mut cache: LruCache<u32, 2> = LruCache::new();
    assert_eq!(cache.put(1), None);
    assert_eq!(cache.put(2), None);
    assert_eq!(cache.put(3), Some(1));
    assert_eq!(cache.peek_mru().copied(), Some(3));
}

#[def_test]
fn test_lru_access_promote() {
    let mut cache: LruCache<u32, 3> = LruCache::new();
    cache.put(1);
    cache.put(2);
    cache.put(3);
    assert!(cache.access(|v| *v == 1));
    let items: alloc::vec::Vec<u32> = cache.items().copied().collect();
    assert_eq!(items.first().copied(), Some(1));
}

#[def_test]
fn test_lru_flush() {
    let mut cache: LruCache<u32, 2> = LruCache::new();
    cache.put(10);
    cache.put(20);
    cache.flush();
    assert!(cache.peek_mru().is_none());
    assert_eq!(cache.items().count(), 0);
}

#[def_test]
fn test_lru_access_miss_and_iteration_order() {
    let mut cache: LruCache<u32, 3> = LruCache::new();
    cache.put(1);
    cache.put(2);
    cache.put(3);

    assert!(!cache.access(|v| *v == 99));
    let items: alloc::vec::Vec<u32> = cache.items().copied().collect();
    assert_eq!(items, alloc::vec![3, 2, 1]);
}

#[def_test]
fn test_lru_put_reuses_lru_slot_after_promotion() {
    let mut cache: LruCache<u32, 2> = LruCache::new();
    cache.put(1);
    cache.put(2);
    assert!(cache.access(|v| *v == 1));

    assert_eq!(cache.put(3), Some(2));
    let items: alloc::vec::Vec<u32> = cache.items().copied().collect();
    assert_eq!(items, alloc::vec![3, 1]);
}
