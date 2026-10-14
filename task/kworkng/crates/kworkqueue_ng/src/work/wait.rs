// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Waiter-owned receipts prepared before capture; no Work or Queue back-reference.

use alloc::{sync::Arc, vec::Vec};
use core::task::Waker;

use kpoll::{PollRegisterError, PollRegistration};
use ktask::future::PreparedTaskWait;

use super::{
    Work,
    observers::{CompletionObserver, PreparedObserver},
};

impl<T, H> Work<T, H> {
    /// Waits for the pending/running occurrences captured under Work protection.
    ///
    /// Does not cancel, disable, execute inline or follow later submissions.
    /// Already detached cleanup is outside this capture. Successful return only
    /// ends this wait's obligations, not admission or the external storage lifetime.
    /// This ordinary wait does not expedite or drain delayed timers.
    /// Destructor-originated and indirect self-waits remain forbidden even though
    /// diagnostics do not guarantee their detection.
    ///
    /// # Errors
    /// Returns invalid context, optional direct self-wait or registration errors
    /// before attaching observers. Idle Work still requires sleepable context.
    ///
    /// # Panics
    /// Propagates fatal allocation and completion invariant failures.
    pub fn flush(&self) -> Result<(), WaitError> {
        let mut task_wait = PreparedTaskWait::prepare().map_err(|_| WaitError::InvalidContext)?;
        #[cfg(feature = "self-wait-check")]
        self.reject_callback_self_wait()?;
        let waker = task_wait.waker();
        let mut wait = loop {
            let mut wait = CleanupWait::prepare(self.observer_count(), &waker)?;
            if self.capture(&mut wait.prepared).is_ok() {
                break wait;
            }
        };
        wait.discard_unused();
        while !wait.is_completed() {
            task_wait.park();
        }
        Ok(())
    }

    #[cfg(feature = "self-wait-check")]
    pub(super) fn reject_callback_self_wait(&self) -> Result<(), WaitError> {
        if self.callback_node.runs_on(ktask::current().owner_key()) {
            Err(WaitError::SelfWait)
        } else {
            Ok(())
        }
    }
}

/// Pre-effect failures of synchronous Work or Queue completion waits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WaitError {
    /// The current context cannot block for completion.
    InvalidContext,
    /// The wait includes the caller's own active callback.
    #[cfg(feature = "self-wait-check")]
    SelfWait,
    /// Completion notification registration failed before wait effects.
    Registration(PollRegisterError),
}

/// Owns completion receipts and their registrations for exactly one captured wait.
pub(super) struct CleanupWait {
    entries: Vec<(Arc<CompletionObserver>, PollRegistration)>,
    pub(super) prepared: Vec<PreparedObserver>,
}

impl CleanupWait {
    pub(super) fn prepare(count: usize, waker: &Waker) -> Result<Self, WaitError> {
        Self::prepare_with(count, |observer| observer.register(waker))
    }

    /// A registration failure drops earlier registrations before any Work capture.
    fn prepare_with(
        count: usize,
        mut register: impl FnMut(&CompletionObserver) -> Result<PollRegistration, PollRegisterError>,
    ) -> Result<Self, WaitError> {
        let mut wait = Self {
            entries: Vec::with_capacity(count),
            prepared: Vec::with_capacity(count),
        };
        for _ in 0..count {
            let (observer, prepared) = CompletionObserver::prepare();
            let registration = register(&observer).map_err(WaitError::Registration)?;
            wait.entries.push((observer, registration));
            wait.prepared.push(prepared);
        }
        Ok(wait)
    }

    /// Capture pops tokens from the end; discard the unused prefix after unlocking.
    pub(super) fn discard_unused(&mut self) {
        let unused = self.prepared.len();
        self.entries.drain(..unused);
        self.prepared.clear();
    }

    pub(super) fn is_completed(&self) -> bool {
        self.entries
            .iter()
            .all(|(receipt, _)| receipt.is_completed())
    }
}

unittest::path_mod_test!(wait_unittest, "../../ut-tests/work_wait_unittest.rs");
