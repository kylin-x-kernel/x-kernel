// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KIRQ synchronization wait provider.

use core::{future::poll_fn, task::Poll};

use kpoll::{Completion, PollRegistrations};

use crate::future::block_on;

#[kiface::provide]
impl kirq::IrqSyncWaitIf {
    fn wait_for_completion(completion: &Completion) -> Result<(), kpoll::PollRegisterError> {
        if completion.try_wait() {
            return Ok(());
        }
        if crate::current_may_uninit().is_none() {
            return Err(kpoll::PollRegisterError::InvalidState);
        }

        let mut registrations = PollRegistrations::new();
        block_on(poll_fn(|cx| {
            if completion.try_wait() {
                return Poll::Ready(Ok(()));
            }

            let mut context = registrations.context(cx);
            if let Err(error) = completion.register(&mut context) {
                return Poll::Ready(Err(error));
            }
            drop(context);

            if completion.try_wait() {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        }))
    }
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test(serial)]
    fn test_kirq_sync_wait_provider_blocks_until_completion() {
        static WAITER_STARTED: AtomicUsize = AtomicUsize::new(0);
        static WAITER_FINISHED: AtomicUsize = AtomicUsize::new(0);

        WAITER_STARTED.store(0, Ordering::Release);
        WAITER_FINISHED.store(0, Ordering::Release);

        let completion = Arc::new(Completion::new());
        let waiter_completion = completion.clone();

        crate::spawn(move || {
            WAITER_STARTED.store(1, Ordering::Release);
            kirq::IrqSyncWaitIf::wait_for_completion(&waiter_completion)
                .expect("completion wait should register from a ktask context");
            WAITER_FINISHED.store(1, Ordering::Release);
        });

        while WAITER_STARTED.load(Ordering::Acquire) == 0 {
            crate::yield_now();
        }
        crate::yield_now();
        assert_eq!(WAITER_FINISHED.load(Ordering::Acquire), 0);

        completion.complete_all();
        while WAITER_FINISHED.load(Ordering::Acquire) == 0 {
            crate::yield_now();
        }
        assert!(completion.is_completed());
    }
}
