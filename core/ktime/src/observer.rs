// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Persistent runtime observers of committed clock changes, independent of I/O waiters.

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, Ordering};

use klazy::Once;
use kspin::SpinNoIrq;
use ktime_types::{BoottimeInstant, MonotonicInstant, SystemTime};

/// Serial identity of a committed clock update, not an expiration count.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct ClockChangeId(u64);
impl ClockChangeId {
    /// Tests serial order. No live arming may span 2^63 clock changes.
    pub fn is_after(self, earlier: Self) -> bool {
        let distance = self.0.wrapping_sub(earlier.0);
        distance != 0 && distance < (1_u64 << 63)
    }
}

/// One consistent timekeeping sample, including its committed change identity.
#[derive(Clone, Copy, Debug)]
pub struct ClockReading {
    /// Current monotonic time.
    pub monotonic: MonotonicInstant,
    /// Corresponding wall-clock time.
    pub realtime: SystemTime,
    /// Corresponding time including sleep.
    pub boottime: BoottimeInstant,
    /// Most recent committed time adjustment.
    pub change_id: ClockChangeId,
}
impl ClockReading {
    pub(super) fn new(sample: super::ClockSnapshot, id: u64) -> Self {
        Self {
            monotonic: sample.monotonic,
            realtime: sample.realtime,
            boottime: sample.boottime,
            change_id: ClockChangeId(id),
        }
    }
}

/// A committed update; samples remain valid even if a subsequent writer overtakes delivery.
#[derive(Clone, Copy, Debug)]
pub struct ClockUpdate {
    /// Clocks sampled under the update's timekeeper write lock.
    pub reading: ClockReading,
    /// Whether this update changes BOOTTIME as well as REALTIME.
    pub includes_boottime: bool,
}

/// IRQ-compatible observer owned by a timer runtime, not by an I/O waiter.
///
/// Calls can overlap and arrive out of order. Account only changes after the
/// timer's arming identity. Never sleep, allocate, unwind or write timekeeping.
/// Implementations and their final Drop must be IRQ-compatible. Waking work
/// must use prepared resources and must not retain borrowed update data.
pub trait ClockObserver: Send + Sync {
    /// Accounts the committed sample; called without timekeeping/registry locks.
    fn on_clock_update(&self, update: ClockUpdate);
}

/// Keeps a runtime observer subscribed. Drop allows one already acquired callback.
/// The observer must close its business state before releasing this registration.
struct Subscription {
    observer: Arc<dyn ClockObserver>,
    active: AtomicBool,
}

/// Owns a runtime subscription. Drop deactivates it without waiting.
/// Already executing callbacks may finish after Drop.
#[must_use = "retain the registration while the runtime is subscribed"]
pub struct ClockObserverRegistration {
    subscription: Arc<Subscription>,
}

struct Registry(SpinNoIrq<Arc<Vec<Weak<Subscription>>>>);
impl Registry {
    fn new() -> Self {
        Self(SpinNoIrq::new(Arc::new(Vec::new())))
    }

    fn register(&self, observer: Arc<dyn ClockObserver>) -> ClockObserverRegistration {
        let subscription = Arc::new(Subscription {
            observer,
            active: AtomicBool::new(true),
        });
        loop {
            let old = {
                let mut current = self.0.lock();
                if let Some(table) = Arc::get_mut(&mut *current)
                    && table.len() < table.capacity()
                {
                    // Spare capacity makes this push allocation-free under the IRQ lock.
                    table.push(Arc::downgrade(&subscription));
                    return ClockObserverRegistration { subscription };
                }
                current.clone()
            };
            // Geometric growth and pruning happen outside the IRQ lock. Holding old
            // prevents in-place writers, so pointer validation also detects races.
            let mut replacement: Vec<_> = old
                .iter()
                .filter(|item| item.strong_count() != 0)
                .cloned()
                .collect();
            // Size growth from live entries: create/close churn must not retain
            // capacity proportional to every subscription ever registered.
            replacement.reserve(replacement.len().max(4));
            replacement.push(Arc::downgrade(&subscription));
            let replacement = Arc::new(replacement);
            let mut current = self.0.lock();
            if Arc::ptr_eq(&old, &current) {
                let retired = core::mem::replace(&mut *current, replacement);
                drop(current);
                drop(retired);
                return ClockObserverRegistration { subscription };
            }
            drop(current);
        }
    }

    fn notify(&self, update: ClockUpdate) {
        let snapshot = self.0.lock().clone();
        for entry in snapshot.iter() {
            if let Some(observer) = entry.upgrade()
                && observer.active.load(Ordering::Acquire)
            {
                observer.observer.on_clock_update(update);
            }
        }
    }
}
static REGISTRY: Once<Registry> = Once::new();

/// Subscribes in allocation-capable task context. No business lock may be held.
/// Registration can allocate; obsolete weak slots are pruned when replacing the table.
/// Final observer destruction may occur in IRQ, including after registration Drop.
pub fn register_clock_observer(observer: Arc<dyn ClockObserver>) -> ClockObserverRegistration {
    REGISTRY.call_once(Registry::new).register(observer)
}
pub(super) fn notify(update: ClockUpdate) {
    if let Some(registry) = REGISTRY.get() {
        registry.notify(update);
    }
}

impl ClockObserverRegistration {
    /// Delivers an isolated fixture sample to this observer only, not global time.
    #[cfg(unittest)]
    pub fn notify_for_test(&self, update: ClockUpdate) {
        self.subscription.observer.on_clock_update(update);
    }
}
impl ClockChangeId {
    /// Produces the next identity for isolated observer tests without clock writes.
    #[cfg(unittest)]
    pub fn next_for_test(self) -> Self {
        Self(self.0.wrapping_add(1))
    }
}

unittest::path_mod_test!(tests, "../ut-tests/observer.rs");

impl Drop for ClockObserverRegistration {
    fn drop(&mut self) {
        self.subscription.active.store(false, Ordering::Release);
    }
}
