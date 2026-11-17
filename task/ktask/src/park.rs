// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared scheduler parking handshake for synchronous and future-based waits.

use core::sync::atomic::{AtomicBool, Ordering};

use kspin::NoPreemptIrqSave;

use crate::{TaskState, current_run_queue};

/// Wake latch shared by a parker and every waker of the same wait.
///
/// Deliberately *not* a spinlock: a parker must never hold a lock across a
/// scheduling point, because the waker of that same wait may itself need the
/// run-queue lock the parker is about to touch. Both sides are lock-free
/// (`record`/`take`), and the parker re-checks the latch after publishing
/// `Blocked`, so a waker that sampled a stale `Running` state and gave up still
/// leaves its wake visible. See `docs/design.md` ("Lock order").
#[derive(Debug, Default)]
pub(crate) struct ParkLatch {
    woke: AtomicBool,
}

impl ParkLatch {
    pub(crate) const fn new() -> Self {
        Self {
            woke: AtomicBool::new(false),
        }
    }

    /// Records a wake; returns whether this call published a new one.
    pub(crate) fn record(&self) -> bool {
        !self.woke.swap(true, Ordering::AcqRel)
    }

    /// Consumes a recorded wake; returns whether one was pending.
    pub(crate) fn take(&self) -> bool {
        self.woke.swap(false, Ordering::AcqRel)
    }
}

/// Parks using a persistent wake latch; the caller retains a strong task ref.
///
/// Call only in sleepable task context. Wakers only record the latch and then
/// enqueue the task; the parker consumes it before and after publishing
/// `Blocked`, so a racing wake either prevents parking or unblocks the task.
/// Returning is only a hint: the caller must recheck its completion or
/// readiness predicate.
pub(crate) fn park_task(latch: &ParkLatch) {
    park_task_unless(latch, TaskState::Blocked, || false, || {});
}

// Recheck cancellation after publishing Blocked: signal wakeup can select a
// different run queue, so checking only before blocked_resched loses a wake.
pub(crate) fn park_task_unless(
    latch: &ParkLatch,
    state: TaskState,
    stop: impl FnOnce() -> bool,
    on_block: impl FnOnce(),
) -> bool {
    if latch.take() {
        return false;
    }

    let mut rq = current_run_queue::<NoPreemptIrqSave>();
    on_block();
    rq.blocked_resched(latch, state, stop);
    true
}
