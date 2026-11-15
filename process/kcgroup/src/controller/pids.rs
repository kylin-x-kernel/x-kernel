// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `pids` controller: process-count admission control.
//!
//! The controller counts tasks, not processes, matching Linux's
//! `pids.current`. New tasks are admitted only while `current < max`; migrated
//! tasks are always admitted (overflow excepted) so a delegated subtree can
//! always be drained.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kerrno::{KError, KResult, LinuxError};

use super::{Controller, ControllerKind};

/// Sentinel stored in `pids.max` for "no limit".
const UNLIMITED: usize = usize::MAX;

/// Largest numeric `pids.max` value accepted by Linux's PID domain.
pub const PIDS_MAX_LIMIT: usize = 4 * 1024 * 1024;

/// Per-cgroup `pids` controller state.
pub struct PidsController {
    is_active: AtomicBool,
    max: AtomicUsize,
    current: AtomicUsize,
    max_hits: AtomicUsize,
}

impl PidsController {
    /// Creates an active controller that already accounts `current` tasks.
    ///
    /// `current` comes from the cgroup's member set when the controller is
    /// enabled late, so enabling a controller never resets the count to zero.
    pub fn new(current: usize) -> Self {
        Self {
            is_active: AtomicBool::new(true),
            max: AtomicUsize::new(UNLIMITED),
            current: AtomicUsize::new(current),
            max_hits: AtomicUsize::new(0),
        }
    }

    /// Returns the numeric limit, or `None` when unlimited.
    pub fn max(&self) -> Option<usize> {
        let max = self.max.load(Ordering::Acquire);
        (max != UNLIMITED).then_some(max)
    }

    /// Stores a numeric limit or clears it with `None`.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` when `max` exceeds [`PIDS_MAX_LIMIT`].
    pub fn set_max(&self, max: Option<usize>) -> KResult<()> {
        let value = match max {
            Some(value) if value > PIDS_MAX_LIMIT => {
                return Err(KError::from(LinuxError::EINVAL));
            }
            Some(value) => value,
            None => UNLIMITED,
        };
        self.max.store(value, Ordering::Release);
        Ok(())
    }

    /// Returns the number of tasks currently charged to this cgroup.
    pub fn current(&self) -> usize {
        self.current.load(Ordering::Acquire)
    }

    /// Returns how many new-task admissions this controller rejected at `max`.
    ///
    /// Migration bypasses the limit and does not change this counter. The
    /// counter belongs to this controller instance, so disabling and
    /// re-enabling the controller starts a new event history at zero.
    pub fn max_hits(&self) -> usize {
        self.max_hits.load(Ordering::Acquire)
    }

    /// Charges a new task, refusing admission once the limit is reached.
    fn reserve_task(&self) -> KResult<()> {
        let mut current = self.current.load(Ordering::Acquire);
        loop {
            if self.is_active.load(Ordering::Acquire) && current >= self.max.load(Ordering::Acquire)
            {
                return Err(KError::from(LinuxError::EAGAIN));
            }
            match self.current.compare_exchange_weak(
                current,
                current
                    .checked_add(1)
                    .ok_or_else(|| KError::from(LinuxError::EAGAIN))?,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    /// Charges a task migrated in from another cgroup, bypassing `max`.
    fn add_migrated_task(&self) -> KResult<()> {
        self.current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(|_| ())
            .map_err(|_| KError::from(LinuxError::EAGAIN))
    }

    /// Releases `count` charged tasks.
    fn release_tasks(&self, count: usize) {
        let result = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(count)
            });
        // A failed decrement indicates a broken membership invariant. Keep the
        // counter unchanged in release builds so corruption cannot wrap it to
        // `usize::MAX` and permanently block admission.
        debug_assert!(result.is_ok(), "cgroup pids count underflow");
    }

    fn disable(&self) {
        self.is_active.store(false, Ordering::Release);
        self.max.store(UNLIMITED, Ordering::Release);
    }
}

impl Controller for PidsController {
    fn kind(&self) -> ControllerKind {
        ControllerKind::Pids
    }

    fn note_fork_failure(&self) {
        self.max_hits.fetch_add(1, Ordering::Relaxed);
    }

    fn fork_charge(&self) -> KResult<()> {
        self.reserve_task()
    }

    fn migrate_in(&self, count: usize) -> KResult<()> {
        for (added, _) in (0..count).enumerate() {
            if let Err(error) = self.add_migrated_task() {
                self.release_tasks(added);
                return Err(error);
            }
        }
        Ok(())
    }

    fn migrate_out(&self, count: usize) {
        self.release_tasks(count);
    }

    fn deactivate(&self) {
        self.disable();
    }
}
