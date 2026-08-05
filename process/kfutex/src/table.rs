// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Permanently allocated, sharded futex waiter table.

use alloc::{collections::VecDeque, sync::Arc, vec::Vec};
use core::{
    future::Future,
    hash::{Hash, Hasher},
    pin::Pin,
    task::{Context, Poll},
};

use kerrno::{KError, KResult};
use kspin::SpinNoPreempt;
use ktask::future::{self, block_on, interruptible};
use ktime_types::TimeSpan;
use kuaccess::{atomic_cmpxchg_u32_nofault, atomic_load_u32, atomic_u32_eq, atomic_u32_eq_nofault};

use crate::{
    FutexKey, FutexWakeOp,
    waiter::{BucketId, FutexWaiter},
};

const BUCKET_COUNT: usize = 256;
const HASH_MIX: u64 = 0x9e37_79b9_7f4a_7c15;

struct FutexBucket {
    waiters: SpinNoPreempt<VecDeque<Arc<FutexWaiter>>>,
}

impl FutexBucket {
    fn new() -> Self {
        Self {
            waiters: SpinNoPreempt::new(VecDeque::with_capacity(8)),
        }
    }
}

/// Global futex waiter table.
///
/// Buckets live for the lifetime of the kernel. Only waiters are allocated,
/// so empty futex keys do not leave reclaimable table entries behind.
pub(crate) struct FutexTable {
    buckets: [FutexBucket; BUCKET_COUNT],
    hash_seed: u64,
}

impl FutexTable {
    fn new() -> Self {
        let seed = khal::time::monotonic_time().as_nanos_u64_saturating() ^ HASH_MIX;
        Self {
            buckets: core::array::from_fn(|_| FutexBucket::new()),
            hash_seed: seed,
        }
    }

    fn bucket_id(&self, key: FutexKey) -> BucketId {
        let mut hash = self.hash_seed;
        key.hash(&mut FutexHasher(&mut hash));
        BucketId((hash as usize) & (BUCKET_COUNT - 1))
    }

    fn bucket(&self, id: BucketId) -> &FutexBucket {
        &self.buckets[id.0]
    }

    /// Blocks while `uaddr` still equals `expected`.
    ///
    /// Returns `false` when the value mismatched before enqueueing and `true`
    /// when a matching wake operation selected this waiter.
    pub(crate) fn wait(
        &'static self,
        key: FutexKey,
        uaddr: usize,
        expected: u32,
        match_mask: u32,
        timeout: Option<TimeSpan>,
    ) -> KResult<bool> {
        if !atomic_u32_eq(uaddr, expected).map_err(KError::from)? {
            return Ok(false);
        }

        block_on(async {
            let mut wait = core::pin::pin!(FutexWaitFuture {
                table: self,
                key,
                bucket: self.bucket_id(key),
                uaddr,
                expected,
                match_mask,
                waiter: None,
            });
            let terminal_error = match interruptible(future::timeout(timeout, wait.as_mut())).await
            {
                Ok(Ok(result)) => return result,
                Ok(Err(elapsed)) => KError::from(elapsed),
                Err(interrupted) => KError::from(interrupted),
            };

            // Signal and timeout selection happen outside the bucket lock. The
            // terminal result is valid only if cancellation wins the same
            // Queued -> Cancelled race used by WAKE. If WAKE won first, report
            // success so one wake cannot be consumed by an error return.
            if wait.as_ref().get_ref().cancel_before_return() {
                Err(terminal_error)
            } else {
                Ok(true)
            }
        })
    }

    /// Wakes at most `count` waiters matching `key` and `mask`.
    pub(crate) fn wake(&self, key: FutexKey, count: usize, mask: u32) -> usize {
        if count == 0 {
            return 0;
        }

        let bucket_id = self.bucket_id(key);
        let mut selected = 0;
        {
            let queue = self.bucket(bucket_id).waiters.lock();
            for waiter in queue.iter() {
                let (waiter_key, ..) = waiter.route_unlocked();
                if selected < count
                    && waiter_key == key
                    && (waiter.match_mask & mask) != 0
                    && waiter.try_wake()
                {
                    selected += 1;
                }
            }
        }
        self.drain_inactive(bucket_id);
        selected
    }

    /// Applies an atomic operation to `uaddr2`, wakes `source_count` waiters,
    /// and conditionally wakes `target_count` waiters.
    pub(crate) fn wake_op(
        &self,
        source: FutexKey,
        target: FutexKey,
        uaddr2: usize,
        source_count: usize,
        target_count: usize,
        operation: FutexWakeOp,
    ) -> KResult<usize> {
        // Fault the word in before taking bucket locks. The locked CAS remains
        // nofault so it cannot sleep while the futex ordering locks are held.
        atomic_load_u32(uaddr2).map_err(KError::from)?;

        let source_id = self.bucket_id(source);
        let target_id = self.bucket_id(target);
        let result = if source_id == target_id {
            let mut queue = self.bucket(source_id).waiters.lock();
            let old = Self::apply_wake_op_locked(uaddr2, operation)?;
            let mut count = Self::mark_wake_locked(&mut queue, source, source_count, u32::MAX);
            if operation.compare(old) {
                count += Self::mark_wake_locked(&mut queue, target, target_count, u32::MAX);
            }
            count
        } else if source_id.0 < target_id.0 {
            let mut source_queue = self.bucket(source_id).waiters.lock();
            let mut target_queue = self.bucket(target_id).waiters.lock();
            let old = Self::apply_wake_op_locked(uaddr2, operation)?;
            let mut count =
                Self::mark_wake_locked(&mut source_queue, source, source_count, u32::MAX);
            if operation.compare(old) {
                count += Self::mark_wake_locked(&mut target_queue, target, target_count, u32::MAX);
            }
            count
        } else {
            let mut target_queue = self.bucket(target_id).waiters.lock();
            let mut source_queue = self.bucket(source_id).waiters.lock();
            let old = Self::apply_wake_op_locked(uaddr2, operation)?;
            let mut count =
                Self::mark_wake_locked(&mut source_queue, source, source_count, u32::MAX);
            if operation.compare(old) {
                count += Self::mark_wake_locked(&mut target_queue, target, target_count, u32::MAX);
            }
            count
        };

        self.drain_inactive(source_id);
        if target_id != source_id {
            self.drain_inactive(target_id);
        }
        Ok(result)
    }

    /// Wakes waiters on `source` and requeues further waiters to `target`.
    ///
    /// When `compare` is present, the comparison is serialized with the
    /// queue operation. A mismatch returns `EAGAIN`.
    pub(crate) fn requeue(
        &self,
        source: FutexKey,
        target: FutexKey,
        wake_count: usize,
        requeue_count: usize,
        compare: Option<(usize, u32)>,
    ) -> KResult<usize> {
        if let Some((uaddr, expected)) = compare {
            if !atomic_u32_eq(uaddr, expected).map_err(KError::from)? {
                return Err(KError::WouldBlock);
            }
            // Fault the compare word in before bucket locks; the locked path
            // remains nofault so it cannot sleep while ordering locks are held.
            atomic_load_u32(uaddr).map_err(KError::from)?;
        }

        let source_id = self.bucket_id(source);
        let target_id = self.bucket_id(target);
        let result = if source_id == target_id {
            let mut queue = self.bucket(source_id).waiters.lock();
            if let Some((uaddr, expected)) = compare {
                Self::compare_locked(uaddr, expected)?;
            }
            Self::requeue_same_bucket(
                &mut queue,
                source,
                target,
                target_id,
                wake_count,
                requeue_count,
            )
        } else if source_id.0 < target_id.0 {
            let mut source_queue = self.bucket(source_id).waiters.lock();
            let mut target_queue = self.bucket(target_id).waiters.lock();
            if let Some((uaddr, expected)) = compare {
                Self::compare_locked(uaddr, expected)?;
            }
            Self::requeue_distinct_buckets(
                &mut source_queue,
                &mut target_queue,
                source,
                target,
                target_id,
                wake_count,
                requeue_count,
            )
        } else {
            let mut target_queue = self.bucket(target_id).waiters.lock();
            let mut source_queue = self.bucket(source_id).waiters.lock();
            if let Some((uaddr, expected)) = compare {
                Self::compare_locked(uaddr, expected)?;
            }
            Self::requeue_distinct_buckets(
                &mut source_queue,
                &mut target_queue,
                source,
                target,
                target_id,
                wake_count,
                requeue_count,
            )
        };

        self.drain_inactive(source_id);
        Ok(result)
    }

    fn compare_locked(uaddr: usize, expected: u32) -> KResult<()> {
        if atomic_u32_eq_nofault(uaddr, expected).map_err(KError::from)? {
            Ok(())
        } else {
            Err(KError::WouldBlock)
        }
    }

    fn apply_wake_op_locked(uaddr: usize, operation: FutexWakeOp) -> KResult<u32> {
        loop {
            let old = kuaccess::atomic_load_u32_nofault(uaddr).map_err(KError::from)?;
            let new = operation.apply(old);
            let (exchanged, _) =
                atomic_cmpxchg_u32_nofault(uaddr, old, new).map_err(KError::from)?;
            if exchanged {
                return Ok(old);
            }
            core::hint::spin_loop();
        }
    }

    fn mark_wake_locked(
        queue: &mut VecDeque<Arc<FutexWaiter>>,
        key: FutexKey,
        count: usize,
        mask: u32,
    ) -> usize {
        let mut selected = 0;
        for waiter in queue.iter() {
            let (waiter_key, ..) = waiter.route_unlocked();
            if selected < count
                && waiter_key == key
                && (waiter.match_mask & mask) != 0
                && waiter.try_wake()
            {
                selected += 1;
            }
        }
        selected
    }

    fn requeue_same_bucket(
        queue: &mut VecDeque<Arc<FutexWaiter>>,
        source: FutexKey,
        target: FutexKey,
        target_id: BucketId,
        wake_count: usize,
        requeue_count: usize,
    ) -> usize {
        let original_len = queue.len();
        let mut woken = 0;
        let mut moved = 0;
        for _ in 0..original_len {
            let waiter = queue
                .pop_front()
                .expect("original futex bucket length must remain available");
            let (key, ..) = waiter.route_unlocked();
            if key == source && waiter.is_queued() {
                if woken < wake_count && waiter.try_wake() {
                    woken += 1;
                } else if moved < requeue_count {
                    waiter.requeue_to(target, target_id);
                    moved += 1;
                }
            }
            queue.push_back(waiter);
        }
        woken + moved
    }

    fn requeue_distinct_buckets(
        source_queue: &mut VecDeque<Arc<FutexWaiter>>,
        target_queue: &mut VecDeque<Arc<FutexWaiter>>,
        source: FutexKey,
        target: FutexKey,
        target_id: BucketId,
        wake_count: usize,
        requeue_count: usize,
    ) -> usize {
        let original_len = source_queue.len();
        let mut woken = 0;
        let mut moved = 0;
        for _ in 0..original_len {
            let waiter = source_queue
                .pop_front()
                .expect("original futex bucket length must remain available");
            let (key, ..) = waiter.route_unlocked();
            if key == source && waiter.is_queued() {
                if woken < wake_count && waiter.try_wake() {
                    woken += 1;
                    source_queue.push_back(waiter);
                    continue;
                }
                if moved < requeue_count {
                    waiter.requeue_to(target, target_id);
                    target_queue.push_back(waiter);
                    moved += 1;
                    continue;
                }
            }
            source_queue.push_back(waiter);
        }
        woken + moved
    }

    /// Cancels a queued waiter.
    ///
    /// Returns `false` when a wake operation won the terminal-state race.
    fn cancel(&self, waiter: &Arc<FutexWaiter>) -> bool {
        loop {
            if waiter.is_woken() {
                return false;
            }
            if !waiter.is_queued() {
                return true;
            }
            let (_, bucket_id, generation) = waiter.route();
            let mut queue = self.bucket(bucket_id).waiters.lock();
            // Must use the locked snapshot: this bucket may be stale if the
            // waiter was already requeued elsewhere, so bucket-lock exclusion
            // alone does not cover `requeue_to` writers.
            let (_, current_bucket, current_generation) = waiter.route();
            if bucket_id != current_bucket || generation != current_generation {
                drop(queue);
                continue;
            }
            if waiter.try_cancel() {
                queue.retain(|queued| !Arc::ptr_eq(queued, waiter));
                return true;
            }
        }
    }

    /// Removes inactive waiters from `bucket_id` and wakes tasks marked `Woken`.
    ///
    /// Runs in a single O(n) pass under the bucket lock, then invokes wakers
    /// after the lock is released (scheduler work must not run while holding
    /// a non-preemptible bucket lock).
    fn drain_inactive(&self, bucket_id: BucketId) {
        let mut to_wake = Vec::new();
        {
            let mut queue = self.bucket(bucket_id).waiters.lock();
            let len = queue.len();
            for _ in 0..len {
                let waiter = queue
                    .pop_front()
                    .expect("futex bucket length must remain available");
                if waiter.is_queued() {
                    queue.push_back(waiter);
                } else if waiter.is_woken() {
                    to_wake.push(waiter);
                }
                // Cancelled waiters are dropped here.
            }
        }
        for waiter in to_wake {
            waiter.wake_task();
        }
    }

    #[cfg(unittest)]
    pub(crate) fn enqueue_waiter_for_test(&self, key: FutexKey, waiter: Arc<FutexWaiter>) {
        let bucket_id = self.bucket_id(key);
        waiter.mark_queued();
        self.bucket(bucket_id).waiters.lock().push_back(waiter);
    }

    #[cfg(unittest)]
    pub(crate) fn cancel_waiter_for_test(&self, waiter: &Arc<FutexWaiter>) -> bool {
        self.cancel(waiter)
    }
}

struct FutexWaitFuture {
    table: &'static FutexTable,
    key: FutexKey,
    bucket: BucketId,
    uaddr: usize,
    expected: u32,
    match_mask: u32,
    waiter: Option<Arc<FutexWaiter>>,
}

impl Future for FutexWaitFuture {
    type Output = KResult<bool>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        if let Some(waiter) = &self.waiter {
            if waiter.is_woken() {
                return Poll::Ready(Ok(true));
            }
            waiter.update_waker(cx.waker());
            return Poll::Pending;
        }

        // Fault the word in before taking the bucket lock. A transient eviction
        // between the syscall-level precheck and enqueue should retry instead
        // of surfacing EFAULT when the mapping is still valid.
        match atomic_u32_eq(self.uaddr, self.expected) {
            Ok(false) => return Poll::Ready(Ok(false)),
            Ok(true) => {}
            Err(err) => return Poll::Ready(Err(KError::from(err))),
        }

        let waiter = Arc::new(FutexWaiter::new(
            self.key,
            self.bucket,
            self.match_mask,
            cx.waker().clone(),
        ));
        {
            let mut queue = self.table.bucket(self.bucket).waiters.lock();
            match atomic_u32_eq_nofault(self.uaddr, self.expected) {
                Ok(true) => {}
                Ok(false) => return Poll::Ready(Ok(false)),
                Err(_) => {
                    drop(queue);
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            }
            waiter.mark_queued();
            queue.push_back(waiter.clone());
        }
        self.waiter = Some(waiter);
        Poll::Pending
    }
}

impl FutexWaitFuture {
    fn cancel_before_return(&self) -> bool {
        self.waiter
            .as_ref()
            .is_none_or(|waiter| self.table.cancel(waiter))
    }
}

impl Drop for FutexWaitFuture {
    fn drop(&mut self) {
        if let Some(waiter) = self.waiter.take() {
            let _ = self.table.cancel(&waiter);
        }
    }
}

struct FutexHasher<'a>(&'a mut u64);

impl Hasher for FutexHasher<'_> {
    fn finish(&self) -> u64 {
        *self.0
    }

    fn write(&mut self, bytes: &[u8]) {
        for byte in bytes {
            *self.0 ^= u64::from(*byte);
            *self.0 = (*self.0).wrapping_mul(HASH_MIX);
            *self.0 ^= *self.0 >> 32;
        }
    }

    fn write_u64(&mut self, value: u64) {
        self.write(&value.to_ne_bytes());
    }

    fn write_usize(&mut self, value: usize) {
        self.write(&value.to_ne_bytes());
    }
}

klazy::lazy_static! {
    static ref FUTEX_TABLE: FutexTable = FutexTable::new();
}

/// Public handle to the process-global futex waiter table.
///
/// The concrete [`FutexTable`] type is crate-private. External callers use this
/// zero-sized handle (from [`global_table`]) so method chaining stays unchanged
/// without exporting the table type name.
#[derive(Clone, Copy, Debug)]
pub struct GlobalFutexTable;

impl GlobalFutexTable {
    /// Blocks while `uaddr` still equals `expected`.
    pub fn wait(
        self,
        key: FutexKey,
        uaddr: usize,
        expected: u32,
        match_mask: u32,
        timeout: Option<TimeSpan>,
    ) -> KResult<bool> {
        FUTEX_TABLE.wait(key, uaddr, expected, match_mask, timeout)
    }

    /// Wakes at most `count` waiters matching `key` and `mask`.
    pub fn wake(self, key: FutexKey, count: usize, mask: u32) -> usize {
        FUTEX_TABLE.wake(key, count, mask)
    }

    /// Applies an atomic operation to `uaddr2`, wakes `source_count` waiters,
    /// and conditionally wakes `target_count` waiters.
    pub fn wake_op(
        self,
        source: FutexKey,
        target: FutexKey,
        uaddr2: usize,
        source_count: usize,
        target_count: usize,
        operation: FutexWakeOp,
    ) -> KResult<usize> {
        FUTEX_TABLE.wake_op(
            source,
            target,
            uaddr2,
            source_count,
            target_count,
            operation,
        )
    }

    /// Wakes waiters on `source` and requeues further waiters to `target`.
    pub fn requeue(
        self,
        source: FutexKey,
        target: FutexKey,
        wake_count: usize,
        requeue_count: usize,
        compare: Option<(usize, u32)>,
    ) -> KResult<usize> {
        FUTEX_TABLE.requeue(source, target, wake_count, requeue_count, compare)
    }
}

/// Returns the system-wide futex waiter table handle.
pub fn global_table() -> GlobalFutexTable {
    GlobalFutexTable
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;
    use core::task::Waker;

    use unittest::def_test;

    use super::FutexTable;
    use crate::{FutexKey, waiter::FutexWaiter};

    fn queued_waiter(key: FutexKey, table: &FutexTable) -> Arc<FutexWaiter> {
        let bucket = table.bucket_id(key);
        Arc::new(FutexWaiter::new(
            key,
            bucket,
            u32::MAX,
            Waker::noop().clone(),
        ))
    }

    #[def_test]
    fn requeue_same_key_wakes_and_keeps_waiters() {
        let table = FutexTable::new();
        let key = FutexKey::private_for_test(1, 0x1000);
        let first = queued_waiter(key, &table);
        let second = queued_waiter(key, &table);
        table.enqueue_waiter_for_test(key, first.clone());
        table.enqueue_waiter_for_test(key, second.clone());

        let count = table
            .requeue(key, key, 1, 1, None)
            .expect("same-key requeue is valid");
        assert_eq!(count, 2);
        assert!(first.is_woken() ^ second.is_woken());
        let kept = if first.is_woken() { &second } else { &first };
        assert_eq!(kept.route().0, key);
        assert!(kept.is_queued());
    }

    #[def_test]
    fn requeue_wakes_one_and_moves_one() {
        let table = FutexTable::new();
        let source = FutexKey::private_for_test(10, 0x1000);
        let target = FutexKey::private_for_test(10, 0x2000);
        let first = queued_waiter(source, &table);
        let second = queued_waiter(source, &table);
        table.enqueue_waiter_for_test(source, first.clone());
        table.enqueue_waiter_for_test(source, second.clone());

        let count = table
            .requeue(source, target, 1, 1, None)
            .expect("requeue waiters");
        assert_eq!(count, 2);
        assert!(first.is_woken() ^ second.is_woken());
        let moved = if first.is_woken() { &second } else { &first };
        assert_eq!(moved.route().0, target);
        assert!(moved.is_queued());
    }

    #[def_test]
    fn cancel_removes_requeued_waiter() {
        let table = FutexTable::new();
        let source = FutexKey::private_for_test(11, 0x3000);
        let target = FutexKey::private_for_test(11, 0x4000);
        let waiter = queued_waiter(source, &table);
        table.enqueue_waiter_for_test(source, waiter.clone());

        table
            .requeue(source, target, 0, 1, None)
            .expect("requeue waiter to target");
        assert_eq!(waiter.route().0, target);
        assert!(waiter.is_queued());

        assert!(table.cancel_waiter_for_test(&waiter));
        assert!(!waiter.is_queued());
        assert!(!waiter.try_cancel());
    }

    #[def_test]
    fn wake_wins_over_late_cancellation() {
        let table = FutexTable::new();
        let key = FutexKey::private_for_test(12, 0x5000);
        let waiter = queued_waiter(key, &table);
        table.enqueue_waiter_for_test(key, waiter.clone());

        assert_eq!(table.wake(key, 1, u32::MAX), 1);
        assert!(!table.cancel_waiter_for_test(&waiter));
        assert!(waiter.is_woken());
    }
}
