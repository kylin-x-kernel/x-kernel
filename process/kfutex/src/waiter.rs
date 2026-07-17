// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Type-owned futex waiter state.

use core::{
    cell::UnsafeCell,
    sync::atomic::{AtomicU8, Ordering},
    task::Waker,
};

use kspin::SpinNoPreempt;

use crate::FutexKey;

const INIT: u8 = 0;
const QUEUED: u8 = 1;
const WOKEN: u8 = 2;
const CANCELLED: u8 = 3;

/// Index of one permanently allocated futex bucket.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct BucketId(pub(crate) usize);

#[derive(Debug, Clone, Copy)]
struct WaiterRoute {
    key: FutexKey,
    bucket: BucketId,
    generation: u64,
}

/// One task blocked by a futex wait operation.
///
/// The waiter is shared by the bucket and the waiting future. It never stores
/// a pointer or reference to a bucket; requeue only changes the checked route.
pub(crate) struct FutexWaiter {
    state: AtomicU8,
    /// Mutated only by [`Self::requeue_to`] while holding bucket lock(s) and
    /// [`Self::route_lock`].
    route: UnsafeCell<WaiterRoute>,
    /// Synchronizes [`Self::route`] (used before the bucket lock is held) with
    /// [`Self::requeue_to`]. Hot paths that already hold the bucket lock use
    /// [`Self::route_unlocked`] instead.
    route_lock: SpinNoPreempt<()>,
    waker: SpinNoPreempt<Waker>,
    pub(crate) match_mask: u32,
}

// SAFETY: `route` is only read/written under `route_lock` or under the
// waiter's current bucket lock(s). Those exclusion sets cover all concurrent
// accessors, so sharing `&FutexWaiter` across threads is sound.
unsafe impl Sync for FutexWaiter {}

impl FutexWaiter {
    pub(crate) fn new(key: FutexKey, bucket: BucketId, match_mask: u32, waker: Waker) -> Self {
        Self {
            state: AtomicU8::new(INIT),
            route: UnsafeCell::new(WaiterRoute {
                key,
                bucket,
                generation: 0,
            }),
            route_lock: SpinNoPreempt::new(()),
            waker: SpinNoPreempt::new(waker),
            match_mask,
        }
    }

    pub(crate) fn mark_queued(&self) {
        self.state.store(QUEUED, Ordering::Release);
    }

    pub(crate) fn is_queued(&self) -> bool {
        self.state.load(Ordering::Acquire) == QUEUED
    }

    pub(crate) fn is_woken(&self) -> bool {
        self.state.load(Ordering::Acquire) == WOKEN
    }

    pub(crate) fn try_wake(&self) -> bool {
        self.state
            .compare_exchange(QUEUED, WOKEN, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn try_cancel(&self) -> bool {
        self.state
            .compare_exchange(QUEUED, CANCELLED, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    pub(crate) fn update_waker(&self, waker: &Waker) {
        let mut current = self.waker.lock();
        if !current.will_wake(waker) {
            *current = waker.clone();
        }
    }

    pub(crate) fn wake_task(&self) {
        let waker = self.waker.lock().clone();
        waker.wake();
    }

    /// Snapshot of the route for callers that do not hold the bucket lock.
    pub(crate) fn route(&self) -> (FutexKey, BucketId, u64) {
        let _guard = self.route_lock.lock();
        // SAFETY: exclusive access via `route_lock`.
        let route = unsafe { &*self.route.get() };
        (route.key, route.bucket, route.generation)
    }

    /// Reads the route without acquiring [`Self::route_lock`].
    ///
    /// The caller must hold the bucket lock for this waiter's current bucket
    /// (both source and target buckets during cross-bucket requeue). That is
    /// the same exclusion set `requeue_to` uses before mutating the route, so
    /// the value cannot change concurrently.
    pub(crate) fn route_unlocked(&self) -> (FutexKey, BucketId, u64) {
        // SAFETY: caller holds the covering bucket lock(s), excluding
        // `requeue_to` writers. Concurrent `route()` readers only observe a
        // stable snapshot under `route_lock` and do not mutate.
        let route = unsafe { &*self.route.get() };
        (route.key, route.bucket, route.generation)
    }

    /// Updates routing while the source and destination bucket locks are held.
    pub(crate) fn requeue_to(&self, key: FutexKey, bucket: BucketId) {
        let _guard = self.route_lock.lock();
        // SAFETY: exclusive access via `route_lock`; caller also holds bucket
        // lock(s) so `route_unlocked` readers on those buckets are excluded.
        let route = unsafe { &mut *self.route.get() };
        route.key = key;
        route.bucket = bucket;
        route.generation = route.generation.wrapping_add(1);
    }
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;
    use core::task::Waker;

    use unittest::def_test;

    use super::{BucketId, FutexWaiter};
    use crate::FutexKey;

    #[def_test]
    fn waiter_route_generation_changes_on_requeue() {
        let source = FutexKey::private_for_test(1, 0x1000);
        let target = FutexKey::private_for_test(1, 0x2000);
        let waiter = Arc::new(FutexWaiter::new(
            source,
            BucketId(3),
            u32::MAX,
            Waker::noop().clone(),
        ));
        waiter.mark_queued();
        assert_eq!(waiter.route(), (source, BucketId(3), 0));

        waiter.requeue_to(target, BucketId(7));
        assert_eq!(waiter.route(), (target, BucketId(7), 1));
        assert!(waiter.try_cancel());
        assert!(!waiter.try_wake());
    }

    #[def_test]
    fn waiter_has_single_terminal_transition() {
        let key = FutexKey::private_for_test(2, 0x3000);
        let waiter = FutexWaiter::new(key, BucketId(1), 0x10, Waker::noop().clone());
        waiter.mark_queued();

        assert!(waiter.try_wake());
        assert!(waiter.is_woken());
        assert!(!waiter.try_cancel());
    }
}
