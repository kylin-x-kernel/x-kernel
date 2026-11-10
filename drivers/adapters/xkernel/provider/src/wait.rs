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
    fn sleep(&self, queue: &WaitQueue) -> Result<(), ResError> {
        let task = ktask::completion::CompletionWait::prepare().map_err(|_| ResError::Busy)?;
        let parked = Arc::new(Parked {
            notified: AtomicBool::new(false),
            wake: task.notifier_waker(),
        });
        let mut woken = false;
        // SAFETY: `parked` and the task live for the whole call; the queue is
        // the driver's own notification source and remains alive across it.
        unsafe {
            queue.wait_until(
                &*parked,
                || {
                    woken = parked.notified.swap(false, Ordering::AcqRel) || woken;
                    woken || task.is_killed()
                },
                || {
                    task.park_lock_killable(queue as *const WaitQueue as usize, || {});
                },
            );
        }
        if task.is_killed() {
            return Err(ResError::Interrupted);
        }
        Ok(())
    }
}

/// The task parking state of one sleep.
struct Parked {
    notified: AtomicBool,
    wake: core::task::Waker,
}

// SAFETY: notification only stores a latch and wakes the prepared task; it
// allocates nothing, retains no borrow and does not reenter the driver queue.
unsafe impl SyncWake for Parked {
    fn notify(&self) {
        self.notified.store(true, Ordering::Release);
        self.wake.wake_by_ref();
    }
}
