// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

extern crate alloc;
use alloc::{format, string::String, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn test_default() {
    assert_eq!(SysnoMap::<u8>::new().count(), 0);
}

#[def_test]
fn test_is_empty() {
    let mut map = SysnoMap::new();
    assert!(map.is_empty());
    assert_eq!(map.insert(Sysno::openat, 42), None);
    assert!(!map.is_empty());
    assert_eq!(map.get(Sysno::openat), Some(&42));
    map.remove(Sysno::openat);
    assert!(map.is_empty());
    assert_eq!(map.get(Sysno::openat), None);
}

#[def_test]
fn test_count() {
    let mut map = SysnoMap::new();
    assert_eq!(map.count(), 0);
    assert_eq!(map.insert(Sysno::openat, 42), None);
    assert_eq!(map.count(), 1);
    assert_eq!(map.insert(Sysno::first(), 4), None);
    assert_eq!(map.count(), 2);
    assert_eq!(map.insert(Sysno::last(), 5), None);
    assert_eq!(map.count(), 3);
    assert_eq!(map.values().sum::<u8>(), 51);
}

#[def_test]
fn test_fn() {
    let mut map = SysnoMap::<fn() -> i32>::new();
    map.insert(Sysno::openat, || 1);
    map.insert(Sysno::close, || -1);
    assert_eq!(map.get(Sysno::openat).unwrap()(), 1);
    assert_eq!(map.get(Sysno::close).unwrap()(), -1);
}

#[def_test]
fn test_fn_macro() {
    type Handler = fn() -> i32;
    let map = SysnoMap::from_iter([
        (Sysno::openat, (|| 1) as Handler),
        (Sysno::close, (|| -1) as Handler),
    ]);
    assert_eq!(map.get(Sysno::openat).unwrap()(), 1);
    assert_eq!(map.get(Sysno::close).unwrap()(), -1);
}

#[def_test]
fn test_insert_remove() {
    let mut map = SysnoMap::new();
    assert_eq!(map.insert(Sysno::openat, 42), None);
    assert!(map.contains_key(Sysno::openat));
    assert_eq!(map.count(), 1);

    assert_eq!(map.insert(Sysno::openat, 4), Some(42));
    assert!(map.contains_key(Sysno::openat));
    assert_eq!(map.count(), 1);

    assert_eq!(map.remove(Sysno::openat), Some(4));
    assert!(!map.contains_key(Sysno::openat));
    assert_eq!(map.count(), 0);

    assert_eq!(map.remove(Sysno::openat), None);
}

#[def_test]
fn test_drop_counts_for_non_copy_values() {
    static DROP_COUNT: AtomicUsize = AtomicUsize::new(0);

    struct DropTracker;

    impl Drop for DropTracker {
        fn drop(&mut self) {
            DROP_COUNT.fetch_add(1, Ordering::Relaxed);
        }
    }

    DROP_COUNT.store(0, Ordering::Relaxed);

    {
        let mut map = SysnoMap::new();

        assert!(map.insert(Sysno::openat, DropTracker).is_none());
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 0);

        let replaced = map.insert(Sysno::openat, DropTracker);
        assert!(replaced.is_some());
        drop(replaced);
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 1);

        let removed = map.remove(Sysno::openat);
        assert!(removed.is_some());
        drop(removed);
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 2);

        assert!(map.insert(Sysno::read, DropTracker).is_none());
        assert!(map.insert(Sysno::close, DropTracker).is_none());
        map.clear();
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 4);

        assert!(map.insert(Sysno::write, DropTracker).is_none());
        assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 4);
    }

    assert_eq!(DROP_COUNT.load(Ordering::Relaxed), 5);
}

#[def_test]
fn test_clear() {
    let mut map = SysnoMap::from_iter([
        (Sysno::read, String::from("read")),
        (Sysno::openat, String::from("openat")),
    ]);
    assert_eq!(map.count(), 2);

    map.clear();

    assert!(map.is_empty());
    assert_eq!(map.count(), 0);
    assert_eq!(map.get(Sysno::read), None);
    assert_eq!(map.get(Sysno::openat), None);
}

#[def_test]
fn test_debug() {
    let map = SysnoMap::from_iter([(Sysno::read, 42), (Sysno::openat, 10)]);
    let result = format!("{:?}", map);
    // The order of the debug output is not guaranteed, so we can't do an
    // exact match.
    assert_eq!(result.len(), "{read: 42, openat: 10}".len());
    assert!(result.starts_with('{'));
    assert!(result.ends_with('}'));
    assert!(result.contains(", "));
    assert!(result.contains("read: 42"));
    assert!(result.contains("openat: 10"));
}

#[def_test]
fn test_iter() {
    let map = SysnoMap::from_iter([(Sysno::read, 42), (Sysno::openat, 10)]);
    assert_eq!(map.iter().collect::<Vec<_>>().len(), 2);
}

#[def_test]
fn test_from_slice_and_values() {
    static CALLBACKS: SysnoMap<u16> =
        SysnoMap::from_slice(&[(Sysno::openat, 7), (Sysno::close, 11), (Sysno::read, 13)]);

    assert_eq!(CALLBACKS.get(Sysno::openat), Some(&7));
    assert_eq!(CALLBACKS.get(Sysno::close), Some(&11));
    assert_eq!(CALLBACKS.get(Sysno::read), Some(&13));
    assert_eq!(CALLBACKS.values().copied().sum::<u16>(), 31);
}

#[def_test]
fn test_into_iter() {
    let map = SysnoMap::from_iter([(Sysno::read, 42), (Sysno::openat, 10)]);
    assert_eq!((&map).into_iter().count(), 2);
}

#[def_test]
fn test_iter_reports_expected_entries() {
    let map = SysnoMap::from_iter([(Sysno::read, 42), (Sysno::openat, 10)]);
    let entries = map.iter().collect::<Vec<_>>();

    assert_eq!(entries.len(), 2);
    assert!(entries.contains(&(Sysno::read, &42)));
    assert!(entries.contains(&(Sysno::openat, &10)));
}

#[def_test]
fn test_init_all() {
    let map = SysnoMap::init_all(&42);
    assert_eq!(map.get(Sysno::openat), Some(&42));
    assert_eq!(map.get(Sysno::close), Some(&42));
}
