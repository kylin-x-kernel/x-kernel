// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`driver_os_api::WaitOp`] provider contract.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use driver_os_api::{ResError, WaitOp};
use kcompletion::{SyncWake, WaitQueue};

use crate::XKernelResourceProvider;

impl WaitOp for XKernelResourceProvider {
    fn sleep(&self, wake: Arc<dyn SyncWake + Send>) -> Result<(), ResError> {
        let task = ktask::completion::CompletionWait::prepare().map_err(|_| ResError::Busy)?;
        // The task is woken through its own notifier, so a driver signal, a
        // task interrupt and a signal that raced the registration all end the
        // park. The task then rechecks its own state and parks again.
        let parked = Arc::new(Parked {
            notified: AtomicBool::new(false),
            wake: task.notifier_waker(),
        });
        let composite = Composite {
            driver: wake,
            parked,
        };
        let mut woken = false;
        // SAFETY: `composite` and the task live for the whole call; the queue
        // is only used to enrol the waiter and the park consumes the latch.
        unsafe {
            let queue = WaitQueue::new();
            queue.wait_until(
                &composite,
                || {
                    // The composite wakes the task before this registration
                    // exists; the latch tells us the wake already arrived.
                    woken = composite.parked.notified.swap(false, Ordering::AcqRel) || woken;
                    woken
                },
                || {
                    task.park_lock_killable(&queue as *const WaitQueue as usize, || {});
                },
            );
        }
        Ok(())
    }
}

/// The task parking state of one sleep.
struct Parked {
    notified: AtomicBool,
    wake: core::task::Waker,
}

impl Parked {
    fn wake_task(&self) {
        self.notified.store(true, Ordering::Release);
        self.wake.wake_by_ref();
    }
}

/// Delivers a driver notification to the parked task as well.
struct Composite {
    driver: Arc<dyn SyncWake + Send>,
    parked: Arc<Parked>,
}

// SAFETY: `notify` only stores a flag and wakes the task; it allocates nothing,
// retains no borrow and never reenters the source it was called from.
unsafe impl SyncWake for Composite {
    fn notify(&self) {
        self.driver.notify();
        self.parked.wake_task();
    }
}
