// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared scheduler parking handshake for synchronous and future-based waits.

use kspin::{NoPreemptIrqSave, SpinNoIrq};

use crate::{TaskState, current_run_queue};

/// Parks using a persistent wake latch; the caller retains a strong task ref.
///
/// Call only in sleepable task context. Wake delivery sets the latch before
/// scheduler enqueue and releases it before taking scheduler locks. Returning
/// is only a hint: the caller must recheck its completion or readiness predicate.
/// Blocked state is published before releasing the latch, so a racing wake
/// either prevents parking or observes a task it can unblock.
pub(crate) fn park_task(woke: &SpinNoIrq<bool>) {
    park_task_unless(woke, TaskState::Blocked, || false, || {});
}

// Recheck cancellation after publishing Blocked: signal wakeup can select a
// different run queue, so checking only before blocked_resched loses a wake.
pub(crate) fn park_task_unless(
    woke: &SpinNoIrq<bool>,
    state: TaskState,
    stop: impl FnOnce() -> bool,
    on_block: impl FnOnce(),
) -> bool {
    {
        let mut woke = woke.lock();
        if *woke {
            *woke = false;
            return false;
        }
    }

    let mut rq = current_run_queue::<NoPreemptIrqSave>();
    let mut woke = woke.lock();
    if *woke {
        *woke = false;
        return false;
    }
    on_block();
    rq.blocked_resched(woke, state, stop);
    true
}
