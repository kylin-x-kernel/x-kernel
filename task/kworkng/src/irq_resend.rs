// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Permanent BH scheduling adapter; IRQ replay ownership stays in KIRQ.

use alloc::sync::Arc;
use core::pin::Pin;

use klazy::Once;

use crate::{DefaultQueue, SubmitError, Work, WorkQueue, services};

static EXECUTOR: Once<Executor> = Once::new();

struct Executor {
    work: Work<fn()>,
    queue: Arc<WorkQueue>,
}

pub(super) fn install() {
    let queue = services::with_default_queue(DefaultQueue::BottomHalf, |queue| queue.cloned());
    let Some(queue) = queue else {
        return;
    };
    EXECUTOR.call_once(|| Executor {
        work: Work::with_callback(kirq::resend::run),
        queue,
    });
    // SAFETY: the private Work and its BH queue remain live for kernel life.
    // notify never calls the callback inline and retains unfinished CPU ownership.
    // KIRQ owns the pending list; no queue lock is held while running its drain.
    let _is_installed = unsafe { kirq::resend::install(notify) };
}

fn notify() {
    let Some(executor) = EXECUTOR.get() else {
        return;
    };
    // SAFETY: the private Once permanently pins Work; it is never canceled,
    // disabled or submitted to another queue. All callbacks are non-sleeping BH.
    let work = unsafe { Pin::new_unchecked(&executor.work) };
    // SAFETY: permanent captures/storage outlive all submissions and execution.
    let result = unsafe { executor.queue.queue_work(work, None) };
    assert!(
        result.is_ok() || matches!(result, Err(SubmitError::AlreadyQueued)),
        "permanent IRQ resend work rejected: {result:?}"
    );
}
