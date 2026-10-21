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
        wait_for_completion(completion)
    }
}

pub(crate) fn wait_for_completion(completion: &Completion) -> Result<(), kpoll::PollRegisterError> {
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

unittest::path_mod_test!(tests, "../ut-tests/irq_wait/tests.rs");
