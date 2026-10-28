// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Suspend-time mapping. Only control-plane transitions write shared cache lines.

use core::sync::atomic::{AtomicBool, AtomicU64, Ordering, fence};

/// Why an explicit monotonic-clock transition could not proceed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum MonotonicSuspendError {
    /// The clock is already suspended.
    AlreadySuspended,
    /// This credential no longer owns the suspended interval.
    StaleToken,
    /// Another CPU is changing the clock mapping.
    TransitionInProgress,
    /// Transition identities cannot be reused in this boot.
    IdentityExhausted,
}

pub(super) struct ClockEpoch {
    sequence: AtomicU64,
    counter_base: AtomicU64,
    logical_base: AtomicU64,
    is_suspended: AtomicBool,
}

impl ClockEpoch {
    pub(super) const fn new() -> Self {
        Self {
            sequence: AtomicU64::new(0),
            counter_base: AtomicU64::new(0),
            logical_base: AtomicU64::new(0),
            is_suspended: AtomicBool::new(false),
        }
    }

    /// The sample is inside the sequence check, not taken before reading bases.
    pub(super) fn read(&self, mut sample: impl FnMut() -> u64) -> (u64, bool) {
        loop {
            let sequence = self.sequence.load(Ordering::Acquire);
            if sequence & 1 != 0 {
                core::hint::spin_loop();
                continue;
            }
            let base = self.logical_base.load(Ordering::Relaxed);
            let counter_base = self.counter_base.load(Ordering::Relaxed);
            let is_suspended = self.is_suspended.load(Ordering::Relaxed);
            let ticks = if is_suspended {
                base
            } else {
                base.saturating_add(sample().wrapping_sub(counter_base))
            };
            // Pair with the writer's release fence: observing any new field must
            // also observe its odd sequence. All payload accesses are atomic,
            // so discarded mixed snapshots have no Rust data race.
            fence(Ordering::Acquire);
            if sequence == self.sequence.load(Ordering::Relaxed) {
                return (ticks, is_suspended);
            }
        }
    }

    fn begin_update(&self) -> Result<u64, MonotonicSuspendError> {
        let sequence = self.sequence.load(Ordering::Relaxed);
        if sequence & 1 != 0 {
            return Err(MonotonicSuspendError::TransitionInProgress);
        }
        if sequence > u64::MAX - 2 {
            return Err(MonotonicSuspendError::IdentityExhausted);
        }
        self.sequence
            .compare_exchange(sequence, sequence + 1, Ordering::AcqRel, Ordering::Relaxed)
            .map_err(|_| MonotonicSuspendError::TransitionInProgress)?;
        // A release fence before relaxed payload writes pairs with readers'
        // acquire fence when they observe any of those payload stores.
        fence(Ordering::Release);
        Ok(sequence)
    }

    pub(super) fn suspend(
        &self,
        sample: impl FnOnce() -> u64,
    ) -> Result<u64, MonotonicSuspendError> {
        let sequence = self.begin_update()?;
        // Reserve both this transition and its eventual resume identity.
        if sequence > u64::MAX - 4 {
            self.sequence.store(sequence, Ordering::Release);
            return Err(MonotonicSuspendError::IdentityExhausted);
        }
        if self.is_suspended.load(Ordering::Relaxed) {
            self.sequence.store(sequence, Ordering::Release);
            return Err(MonotonicSuspendError::AlreadySuspended);
        }
        let ticks = self
            .logical_base
            .load(Ordering::Relaxed)
            .saturating_add(sample().wrapping_sub(self.counter_base.load(Ordering::Relaxed)));
        self.logical_base.store(ticks, Ordering::Relaxed);
        self.is_suspended.store(true, Ordering::Relaxed);
        self.sequence.store(sequence + 2, Ordering::Release);
        Ok(sequence + 2)
    }

    pub(super) fn resume(
        &self,
        identity: u64,
        sample: impl FnOnce() -> u64,
    ) -> Result<(), MonotonicSuspendError> {
        let sequence = self.begin_update()?;
        if sequence != identity || !self.is_suspended.load(Ordering::Relaxed) {
            // No payload changed; restoring the old sequence preserves the
            // valid owner's credential after a rejected stale request.
            self.sequence.store(sequence, Ordering::Release);
            return Err(MonotonicSuspendError::StaleToken);
        }
        self.counter_base.store(sample(), Ordering::Relaxed);
        self.is_suspended.store(false, Ordering::Relaxed);
        self.sequence.store(sequence + 2, Ordering::Release);
        Ok(())
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../../ut-tests/time_epoch.rs");
