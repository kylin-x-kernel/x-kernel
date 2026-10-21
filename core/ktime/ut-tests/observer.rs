// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;

struct Counter(Arc<AtomicUsize>);
impl ClockObserver for Counter {
    fn on_clock_update(&self, _: ClockUpdate) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}
#[def_test]
fn persistent_observer_accounts_every_update_and_unsubscribes() {
    let registry = Registry::new();
    let count = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(Counter(count.clone()));
    let subscription = registry.register(owner.clone());
    let update = ClockUpdate {
        reading: super::super::clock_reading(),
        includes_boottime: true,
    };
    registry.notify(update);
    registry.notify(update);
    assert_eq!(count.load(Ordering::Relaxed), 2);
    drop(subscription);
    // Retaining the observer itself must not retain its subscription.
    registry.notify(update);
    assert_eq!(count.load(Ordering::Relaxed), 2);
    drop(owner);
}
#[def_test]
fn exclusive_registrations_reuse_storage_and_deliver_all_observers() {
    let registry = Registry::new();
    let count = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(Counter(count.clone()));
    let mut registrations = Vec::new();
    let mut previous = None;
    let mut replacements = 0;
    for _ in 0..128 {
        registrations.push(registry.register(owner.clone()));
        let pointer = Arc::as_ptr(&registry.0.lock());
        if previous != Some(pointer) {
            replacements += 1;
        }
        previous = Some(pointer);
    }
    // Bound table replacement frequency, not wall time or allocator addresses.
    // Unconditional COW would replace the live table on all 128 registrations.
    assert!(replacements <= 8);
    registry.notify(ClockUpdate {
        reading: super::super::clock_reading(),
        includes_boottime: true,
    });
    assert_eq!(count.load(Ordering::Relaxed), 128);
    drop(registrations);
    registry.notify(ClockUpdate {
        reading: super::super::clock_reading(),
        includes_boottime: true,
    });
    assert_eq!(count.load(Ordering::Relaxed), 128);
}
#[def_test]
fn registration_close_churn_keeps_table_storage_bounded() {
    let registry = Registry::new();
    let count = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(Counter(count.clone()));
    for _ in 0..4096 {
        drop(registry.register(owner.clone()));
    }
    {
        let table = registry.0.lock();
        assert!(table.len() <= 4);
        assert!(table.capacity() <= 8);
    }
    registry.notify(ClockUpdate {
        reading: super::super::clock_reading(),
        includes_boottime: true,
    });
    assert_eq!(count.load(Ordering::Relaxed), 0);
}
#[def_test]
fn registration_preserves_acquired_notification_snapshot() {
    let registry = Registry::new();
    let count = Arc::new(AtomicUsize::new(0));
    let owner = Arc::new(Counter(count.clone()));
    let first = registry.register(owner.clone());
    let snapshot = registry.0.lock().clone();
    let second = registry.register(owner);
    assert_eq!(snapshot.len(), 1);
    assert_eq!(registry.0.lock().len(), 2);
    assert!(!Arc::ptr_eq(&snapshot, &registry.0.lock()));
    registry.notify(ClockUpdate {
        reading: super::super::clock_reading(),
        includes_boottime: true,
    });
    assert_eq!(count.load(Ordering::Relaxed), 2);
    drop(first);
    drop(second);
}
#[def_test]
fn clock_serial_order_includes_wrap_but_excludes_old_samples() {
    let before = ClockChangeId(u64::MAX);
    let after = ClockChangeId(0);
    assert!(after.is_after(before));
    assert!(!before.is_after(after));
    assert!(!after.is_after(after));
}
