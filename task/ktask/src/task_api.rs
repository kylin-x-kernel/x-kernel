// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Providers for shared task execution, blocking admission and completion waits.

use core::{future::Future, num::NonZeroU64, pin::Pin};

#[cfg(unittest)]
use kmutex::Mutex;

#[kiface::provide]
impl kiface_task_api::TaskBlockingContextIf {
    fn is_sleepable_context() -> bool {
        crate::is_sleepable_context()
    }
}

impl kiface_task_wait_api::PreparedCompletionWait for crate::completion::CompletionWait {
    fn wait_for_completion(&self, completion: &kcompletion::Completion) {
        self.wait(completion);
    }
}

#[kiface::provide]
impl kiface_task_wait_api::TaskCompletionWaitIf {
    fn with_prepared_wait(
        body: &mut dyn FnMut(&mut dyn kiface_task_wait_api::PreparedCompletionWait),
    ) -> kerrno::KResult<()> {
        let mut wait = crate::completion::CompletionWait::prepare()?;
        body(&mut wait);
        Ok(())
    }
}

#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: holder_key uses a nonzero identity token or \
              the non-null address of the live task"
)]
fn owner_key_for_task(task: &crate::TaskInner) -> NonZeroU64 {
    NonZeroU64::new(crate::lock_owner::holder_key(task))
        .expect("live task has a nonzero ownership key")
}

#[kiface::provide]
impl kiface_task_api::TaskExecutionIf {
    fn current_owner_key() -> NonZeroU64 {
        owner_key_for_task(&crate::current())
    }

    fn yield_now() {
        crate::yield_now();
    }

    fn block_on(waiter: Pin<&mut dyn Future<Output = ()>>) {
        crate::future::block_on(waiter);
    }
}

#[kiface::provide]
impl kiface_task_api::TaskLockDiagnosticsIf {
    fn lock_acquired(address: usize) {
        #[cfg(feature = "snapshot")]
        {
            let task = crate::current();
            task.clear_waiting_lock();
            task.push_held_lock(address);
        }
        #[cfg(not(feature = "snapshot"))]
        let _ = address;
    }

    fn lock_waiting(address: usize) {
        #[cfg(feature = "snapshot")]
        crate::current().set_waiting_lock(address, khal::time::now_ticks());
        #[cfg(not(feature = "snapshot"))]
        let _ = address;
    }

    fn lock_released(address: usize) {
        #[cfg(feature = "snapshot")]
        crate::current().pop_held_lock(address);
        #[cfg(not(feature = "snapshot"))]
        let _ = address;
    }
}

unittest::path_mod_test!(tests, "../ut-tests/task_api.rs");

impl kiface_task_wait_api::PreparedParking for crate::future::PreparedTaskWait {
    fn waker(&self) -> core::task::Waker {
        self.waker()
    }

    fn park(&mut self) {
        self.park();
    }
}
#[kiface::provide]
impl kiface_task_wait_api::TaskParkingIf {
    fn with_prepared_parking(
        body: &mut dyn FnMut(&mut dyn kiface_task_wait_api::PreparedParking),
    ) -> kerrno::KResult<()> {
        let mut wait = crate::future::PreparedTaskWait::prepare()?;
        body(&mut wait);
        Ok(())
    }
}

pub(crate) fn task_needs_resched(task: Option<&crate::TaskInner>) -> bool {
    task.is_some_and(crate::TaskInner::is_preempt_pending)
}
