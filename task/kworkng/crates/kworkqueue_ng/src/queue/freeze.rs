// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reversible dispatch exclusion, independent of ordinary admission and flush colors.

use alloc::sync::Arc;
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, Ordering},
};

use kerrno::KError;
use ktask::completion::CompletionWait;
use ktime_types::TimeSpan;

use super::Queue;

/// A freeze attempt failed; any installed dispatch pauses have been released.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueFreezeError {
    /// Requires initialized sleepable task context with timer IRQ progress.
    InvalidContext,
    /// The deadline cannot be represented.
    InvalidTimeout,
    /// The queue is permanently closed.
    Closed,
    /// Another attempt or successful guard still owns dispatch exclusion.
    Busy,
    /// The one-shot cancellation handle was cancelled before success.
    Cancelled,
    /// A previously claimed execution did not finish before the deadline.
    TimedOut,
    /// Preparing the deadline timer failed before dispatch was paused.
    Timer(KError),
    /// The caller is executing a callback belonging to this queue.
    #[cfg(feature = "self-wait-check")]
    SelfWait,
}

pub(super) struct FreezeProgress {
    is_cancelled: AtomicBool,
    pub(super) changed: kcompletion::WaitQueue,
}

/// One-shot cancellation for unfinished queue freezes; prepare in task context.
/// Clones may cancel from task or IRQ context. Use a fresh handle for a retry.
#[derive(Clone)]
pub struct QueueFreezeCancellation(Arc<FreezeProgress>);

impl Default for QueueFreezeCancellation {
    fn default() -> Self {
        Self::new()
    }
}

impl QueueFreezeCancellation {
    /// Allocates shared progress state before entering PM or taking queue locks.
    pub fn new() -> Self {
        Self(Arc::new(FreezeProgress {
            is_cancelled: AtomicBool::new(false),
            changed: kcompletion::WaitQueue::new(),
        }))
    }

    /// Wakes unfinished attempts. Does not thaw a successfully returned guard.
    pub fn cancel(&self) {
        self.0.is_cancelled.store(true, Ordering::Release);
        self.0.changed.notify_all();
    }
}

/// Holds dispatch paused after all previously claimed callbacks and occurrence
/// resource cleanup have finished. Pending work and timer arrangements survive.
/// Drop in ordinary task context after resources needed by callbacks are usable.
/// This guard is task-bound and retains the queue; dropping other handles cannot
/// release it. Forgetting it leaves dispatch paused. It grants no Work lifetime.
#[must_use = "retain until callbacks may safely access their resources again"]
pub struct FrozenQueue {
    queue: Arc<Queue>,
    _task_bound: PhantomData<*mut ()>,
}

impl Drop for FrozenQueue {
    fn drop(&mut self) {
        for partition in &self.queue.partitions {
            partition.thaw();
        }
        *self.queue.is_freezing.lock() = false;
    }
}

impl Queue {
    /// Pauses this explicitly selected queue and waits for claimed executions.
    ///
    /// The per-partition claim lock is the cutoff: a prior claim is joined even
    /// if its provider has not entered the callback yet. New submissions, delayed
    /// timer-to-queue transfers, cancellations and callback self-requeues retain
    /// their ordinary rules, but no further callback can be claimed until Drop.
    /// Shared execution providers and other queues continue to run.
    ///
    /// The bound covers callbacks and their occurrence payload cleanup, not
    /// observer notification, provider retirement actions, physical timer delivery,
    /// IRQ/BH producers or other queues. It does not freeze tasks or clocks.
    /// The caller must arrange those PM dependencies separately. Release the
    /// guard before waiting on pending work, flush or close_and_drain; those
    /// operations can otherwise wait for work intentionally held by this guard.
    /// Never wait from this queue's own callback or while holding its resources.
    ///
    /// Requires initialized sleepable task context and advancing MONOTONIC/timer
    /// IRQs. Cancellation/timeout rolls back every partition pause. Cancellation
    /// after success cannot release dispatch. The same queue has one owner;
    /// unrelated queues can freeze concurrently.
    ///
    /// # Errors
    /// Returns context, deadline/timer, closed, busy, cancellation or timeout
    /// errors. Optional self-wait diagnostics reject direct callback waits.
    /// Every ordinary error leaves no pause owned by this attempt.
    ///
    /// # Panics
    /// Propagates fatal allocation and violated initialized timer/provider invariants.
    pub fn freeze(
        self: &Arc<Self>,
        timeout: TimeSpan,
        cancellation: &QueueFreezeCancellation,
    ) -> Result<FrozenQueue, QueueFreezeError> {
        let wait = CompletionWait::prepare().map_err(|_| QueueFreezeError::InvalidContext)?;
        #[cfg(feature = "self-wait-check")]
        if self.reject_callback_self_wait().is_err() {
            return Err(QueueFreezeError::SelfWait);
        }
        let deadline = khal::time::monotonic_time()
            .checked_add(timeout)
            .ok_or(QueueFreezeError::InvalidTimeout)?;
        let progress = cancellation.0.clone();
        let timer = ktask::Timer::new(move |_: ktask::TimerControl<'_>, _: ktask::Expiration| {
            progress.changed.notify_all();
        });
        if khal::time::monotonic_time() < deadline {
            // SAFETY: sleepable admission requires initialized scheduler/timer
            // context. The owned handler only performs IRQ-safe, allocation-free
            // notification. It never sleeps, unwinds, enables IRQs or cancels
            // itself. Timer Drop drains delivery here before PM may stop clocks.
            unsafe { timer.start(deadline) }.map_err(QueueFreezeError::Timer)?;
        }
        if cancellation.0.is_cancelled.load(Ordering::Acquire) {
            return Err(QueueFreezeError::Cancelled);
        }
        {
            let mut is_freezing = self.is_freezing.lock();
            if *is_freezing {
                return Err(QueueFreezeError::Busy);
            }
            if self.admission_color().is_none() {
                return Err(QueueFreezeError::Closed);
            }
            *is_freezing = true;
        }
        let guard = FrozenQueue {
            queue: self.clone(),
            _task_bound: PhantomData,
        };
        for partition in &self.partitions {
            partition.begin_freeze(&cancellation.0);
        }
        let is_quiescent = || {
            self.partitions
                .iter()
                .all(|partition| partition.is_quiescent())
        };
        wait.wait_until(&cancellation.0.changed, || {
            cancellation.0.is_cancelled.load(Ordering::Acquire)
                || is_quiescent()
                || khal::time::monotonic_time() >= deadline
        });
        if cancellation.0.is_cancelled.load(Ordering::Acquire) {
            return Err(QueueFreezeError::Cancelled);
        }
        if !is_quiescent() {
            return Err(QueueFreezeError::TimedOut);
        }
        Ok(guard)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/queue_freeze_unittest.rs");
