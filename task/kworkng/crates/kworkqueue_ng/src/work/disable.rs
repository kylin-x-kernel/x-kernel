// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Independent ordinary-Work disable ownership, not exclusive business-data access.

use ktask::future::PreparedTaskWait;

use super::{
    Work,
    wait::{CleanupWait, WaitError},
};

/// Borrows external Work while holding one independent submission disable.
///
/// Drop releases only this caller's disable; the last release permits future
/// submission without restoring canceled work or waiting for running execution.
/// Forgetting a guard leaves admission disabled, not a dangling stored reference.
#[must_use = "dropping this guard releases its submission disable"]
pub struct DisableGuard<'a, T, H> {
    work: &'a Work<T, H>,
}

impl<T, H> Work<T, H> {
    /// Cancels ordinary pending work and waits for the captured running cleanup.
    ///
    /// Releases only this call's temporary disable before returning. Concurrent
    /// producers may then submit again; callers must stop/join producers before
    /// releasing storage. Already detached cancellation cleanup is outside capture.
    /// With diagnostics enabled, rejects the currently executing callback itself.
    /// Requires sleepable context; this ordinary operation is not a delayed drain.
    ///
    /// # Errors
    /// Returns context, identity-validation or registration errors before effects.
    ///
    /// # Panics
    /// Propagates fatal preparation, cleanup and disable-depth invariant failures.
    pub fn cancel_sync(&self) -> Result<bool, WaitError> {
        let (guard, revoked) = self.disable_sync()?;
        drop(guard);
        Ok(revoked)
    }

    /// Holds one independent disable while canceling and draining the captured Work.
    ///
    /// Prepares the task waker and all observers before capture. Capacity is
    /// rechecked under Work protection, and capture plus disable share that guard.
    /// Cancellation and waiting run unlocked. The returned guard keeps admission
    /// disabled until its owner drops it. Optional self-wait diagnostics compare
    /// the running callback's task identity, not its current CPU. Indirect waits
    /// and destructor-originated self-waits remain invalid without guaranteed diagnosis.
    ///
    /// # Errors
    /// Invalid sleepable context (even for idle Work), self-wait validation and
    /// registration failures return before changing Work state.
    ///
    /// # Panics
    /// Propagates fatal allocation/destructor/Waker failure and disable-depth overflow.
    pub fn disable_sync(&self) -> Result<(DisableGuard<'_, T, H>, bool), WaitError> {
        let mut task_wait = PreparedTaskWait::prepare().map_err(|_| WaitError::InvalidContext)?;
        #[cfg(feature = "self-wait-check")]
        self.reject_callback_self_wait()?;
        let waker = task_wait.waker();
        let (mut wait, guard) = loop {
            let mut wait = CleanupWait::prepare(self.observer_count(), &waker)?;
            let mut state = self.state.lock();
            if state.observer_count() > wait.prepared.len() {
                drop(state);
                continue;
            }
            state.begin_disable();
            state.capture(&mut wait.prepared);
            drop(state);
            break (wait, DisableGuard { work: self });
        };
        wait.discard_unused();
        let revoked = self.cancel_pending();
        while !wait.is_completed() {
            task_wait.park();
        }
        Ok((guard, revoked))
    }

    /// Disables new submission and revokes ordinary pending work without waiting.
    ///
    /// The bool reports actual pending revocation, not whether Work was idle.
    /// Claim racing cancellation can win and remains running. Closing admission
    /// first prevents a replacement from escaping cancellation; Work protection
    /// ends before cancel acquires partition -> Work. Detached cleanup is performed
    /// unlocked before return and must obey the calling context.
    /// This is not synchronous disable and does not cover a future delayed timer.
    ///
    /// # Panics
    /// Panics on disable-depth overflow; that failure has no state effect. Also
    /// propagates resource-destructor or completion-Waker panics during revocation.
    pub fn disable(&self) -> (DisableGuard<'_, T, H>, bool) {
        self.state.lock().begin_disable();
        let guard = DisableGuard { work: self };
        let revoked = self.cancel_pending();
        (guard, revoked)
    }
}

impl<T, H> Drop for DisableGuard<'_, T, H> {
    fn drop(&mut self) {
        self.work.state.lock().end_disable();
    }
}

#[cfg(unittest)]
mod tests;
