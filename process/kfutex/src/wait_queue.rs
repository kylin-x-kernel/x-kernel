// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::vec_deque::VecDeque, sync::Arc, vec::Vec};
use core::{
    future::Future,
    pin::Pin,
    sync::atomic::{AtomicBool, Ordering},
    task::{Context, Poll, Waker},
    time::Duration,
};

use kerrno::KResult;
use kspin::SpinNoIrq;
use ktask::future::{self, block_on, interruptible};

/// Wait queue used by futex.
#[derive(Default)]
pub struct WaitQueue {
    queue: SpinNoIrq<VecDeque<Waiter>>,
}

struct Waiter {
    waker: Waker,
    bitset: u32,
    is_active: Arc<AtomicBool>,
}

struct WaitFuture<'a, C> {
    wait_queue: &'a WaitQueue,
    bitset: u32,
    condition: Option<C>,
    waiter: Option<Arc<AtomicBool>>,
}

impl<C> Unpin for WaitFuture<'_, C> {}

impl<C: FnOnce() -> bool> Future for WaitFuture<'_, C> {
    type Output = KResult<bool>;

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = &mut *self;

        if let Some(token) = this.waiter.clone() {
            let mut queue = this.wait_queue.queue.lock();
            if let Some(waiter) = queue
                .iter_mut()
                .find(|waiter| Arc::ptr_eq(&waiter.is_active, &token))
            {
                if !waiter.is_active.load(Ordering::Acquire) {
                    queue.retain(|waiter| !Arc::ptr_eq(&waiter.is_active, &token));
                    this.waiter = None;
                    return Poll::Ready(Ok(true));
                }
                waiter.waker = cx.waker().clone();
                return Poll::Pending;
            }

            if token.load(Ordering::Acquire) {
                return Poll::Pending;
            }

            this.waiter = None;
            return Poll::Ready(Ok(true));
        }

        let Some(condition) = this.condition.take() else {
            return Poll::Ready(Ok(true));
        };

        // Enqueue ourselves BEFORE checking the condition, holding the wait-queue
        // lock only for the push. The push touches no user memory, so it cannot
        // page-fault or block while preemption is disabled.
        let token = Arc::new(AtomicBool::new(true));
        {
            let mut queue = this.wait_queue.queue.lock();
            queue.push_back(Waiter {
                waker: cx.waker().clone(),
                bitset: this.bitset,
                is_active: token.clone(),
            });
        }
        this.waiter = Some(token);

        // Evaluate the condition outside the preempt-disabled lock. `condition` may
        // access user memory (e.g. a futex word) and page-fault; resolving that
        // fault can block on the address-space lock, which requires preemption
        // enabled (blocked_resched asserts preempt_disable_count == 2).
        //
        // Because we are already enqueued with is_active == true, a concurrent
        // wake() that matches our bitset deactivates us and re-schedules this
        // future, so the wakeup is never lost. If the condition holds we return
        // Pending and wait to be woken; if it already fails, `Drop` (this.waiter
        // is Some) deactivates and removes our waiter, reporting "no need to wait".
        if condition() {
            Poll::Pending
        } else {
            Poll::Ready(Ok(false))
        }
    }
}

impl<C> Drop for WaitFuture<'_, C> {
    fn drop(&mut self) {
        if let Some(token) = self.waiter.take() {
            token.store(false, Ordering::Release);
            self.wait_queue.remove_waiter(&token);
        }
    }
}

impl WaitQueue {
    /// Creates a new `WaitQueue`.
    pub fn new() -> Self {
        Self::default()
    }

    fn remove_waiter(&self, token: &Arc<AtomicBool>) {
        self.queue
            .lock()
            .retain(|waiter| !Arc::ptr_eq(&waiter.is_active, token));
    }

    /// Waits if the given condition is met.
    ///
    /// Returns `false` if the condition is not met and no actual waiting
    /// occurs.
    pub fn wait_if(
        &self,
        bitset: u32,
        timeout: Option<Duration>,
        condition: impl FnOnce() -> bool,
    ) -> KResult<bool> {
        block_on(interruptible(future::timeout(
            timeout,
            WaitFuture {
                wait_queue: self,
                bitset,
                condition: Some(condition),
                waiter: None,
            },
        )))??
    }

    /// Wakes up at most `count` tasks whose bitset intersects with the given
    /// bitmask.
    pub fn wake(&self, count: usize, mask: u32) -> usize {
        let mut woke = 0;
        self.queue.lock().retain(|waiter| {
            if !waiter.is_active.load(Ordering::Acquire) {
                false
            } else if woke >= count || (waiter.bitset & mask) == 0 {
                true
            } else {
                waiter.is_active.store(false, Ordering::Release);
                waiter.waker.wake_by_ref();
                woke += 1;
                false
            }
        });
        woke
    }

    /// Checks if the wait queue is empty.
    pub fn is_empty(&self) -> bool {
        let mut queue = self.queue.lock();
        queue.retain(|waiter| waiter.is_active.load(Ordering::Acquire));
        queue.is_empty()
    }

    /// Requeue at most `count` tasks to the target wait queue.
    pub fn requeue(&self, mut count: usize, target: &WaitQueue) -> usize {
        let tasks = {
            let mut wq = self.queue.lock();
            let mut tasks = Vec::new();
            let mut remaining = VecDeque::new();

            while let Some(waiter) = wq.pop_front() {
                if !waiter.is_active.load(Ordering::Acquire) {
                    continue;
                }
                if tasks.len() < count {
                    tasks.push(waiter);
                } else {
                    remaining.push_back(waiter);
                }
            }

            count = tasks.len();
            *wq = remaining;
            tasks
        };
        if !tasks.is_empty() {
            let mut wq = target.queue.lock();
            wq.extend(tasks);
        }
        count
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::WaitQueue;

    #[def_test]
    fn test_waitqueue_wake_empty() {
        let wq = WaitQueue::new();
        assert!(wq.is_empty());
        assert_eq!(wq.wake(1, 0xffff_ffff), 0);
    }

    #[def_test]
    fn test_waitqueue_requeue_empty() {
        let src = WaitQueue::new();
        let dst = WaitQueue::new();
        assert_eq!(src.requeue(1, &dst), 0);
        assert!(src.is_empty());
        assert!(dst.is_empty());
    }
}
