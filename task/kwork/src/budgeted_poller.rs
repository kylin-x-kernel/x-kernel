// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Coalesced budgeted poller.
//!
//! This module provides the NAPI-like executor shape used by subsystems which
//! need missed-wake coalescing, bounded poll rounds, and optional foreground
//! assist. A budgeted poller is a long-lived execution object backed by normal
//! work items; it is not an async task executor.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kspin::SpinNoIrq;

use crate::{
    QueueWorkResult, ScheduledWork, WorkQueueAllocError, WorkQueueAttrs, WorkQueueHandle,
    WorkqueueError,
};

/// Result of one budgeted poll round.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct BudgetedPollProgress {
    /// Whether work is ready for immediate processing in another round.
    pub has_more: bool,
}

/// Error returned when a [`BudgetedPoller`] cannot be started.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BudgetedPollerStartError {
    /// Poller has been destroyed and cannot be restarted.
    Destroyed,
    /// Poller start was attempted from an interrupt-like context.
    InvalidContext,
    /// The requested lane policy is not implemented by the current runtime.
    UnsupportedPolicy,
}

impl From<WorkQueueAllocError> for BudgetedPollerStartError {
    fn from(error: WorkQueueAllocError) -> Self {
        match error {
            WorkQueueAllocError::InvalidContext => Self::InvalidContext,
            WorkQueueAllocError::UnsupportedFlags => Self::UnsupportedPolicy,
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
#[repr(u8)]
enum BudgetedPollerState {
    Idle           = 0b00,
    Scheduled      = 0b01,
    Running        = 0b10,
    RunningPending = 0b11,
}

impl BudgetedPollerState {
    const MASK: usize = 0b11;
    const PUBLISH_GENERATION_STEP: usize = 0b100;

    fn from_raw(value: usize) -> Self {
        match value & Self::MASK {
            0b00 => Self::Idle,
            0b01 => Self::Scheduled,
            0b10 => Self::Running,
            0b11 => Self::RunningPending,
            _ => unreachable!("invalid budgeted poller state"),
        }
    }
}

struct BudgetedPollerInner<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    name: &'static str,
    started: AtomicBool,
    is_destroying: AtomicBool,
    queue: SpinNoIrq<Option<WorkQueueHandle>>,
    works: [ScheduledWork; 2],
    state: AtomicUsize,
    background_budget: B,
    assist_budget: B,
    max_background_rounds: usize,
    poll_once: F,
}

impl<B, F> BudgetedPollerInner<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    fn start(&self) -> Result<(), BudgetedPollerStartError> {
        if self.is_destroying.load(Ordering::Acquire) {
            return Err(BudgetedPollerStartError::Destroyed);
        }

        if self.started.swap(true, Ordering::AcqRel) {
            return Ok(());
        }

        let attrs = WorkQueueAttrs::new().with_max_active(1);
        match WorkQueueHandle::alloc(self.name, attrs) {
            Ok(queue) => {
                let mut queue_slot = self.queue.lock();
                if self.is_destroying.load(Ordering::Acquire) {
                    self.started.store(false, Ordering::Release);
                    return Err(BudgetedPollerStartError::Destroyed);
                }
                *queue_slot = Some(queue);
                drop(queue_slot);
                if self.load_state() != BudgetedPollerState::Idle {
                    let _ = self.queue_background_work(0);
                }
                Ok(())
            }
            Err(error) => {
                self.started.store(false, Ordering::Release);
                Err(error.into())
            }
        }
    }

    fn notify_irq_safe(&self) -> bool {
        if self.is_destroying.load(Ordering::Acquire) {
            return false;
        }

        let previous = self.publish_work();
        match BudgetedPollerState::from_raw(previous) {
            BudgetedPollerState::Idle => {
                let published = Self::published_raw(previous);
                if self.queue_background_work(0) {
                    true
                } else {
                    if self.started.load(Ordering::Acquire) {
                        self.clear_scheduled_if_unchanged(published);
                    }
                    false
                }
            }
            BudgetedPollerState::Scheduled => self.queue_background_work(0),
            BudgetedPollerState::Running | BudgetedPollerState::RunningPending => true,
        }
    }

    fn publish_work(&self) -> usize {
        // The low bit means pending work in both execution modes. The high
        // bits are a publish generation used to avoid clearing `Scheduled`
        // after a failed queue attempt when a concurrent producer also
        // published work.
        self.state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |raw| {
                Some(Self::published_raw(raw))
            })
            .expect("budgeted poller publish update should not fail")
    }

    fn published_raw(raw: usize) -> usize {
        let state = match BudgetedPollerState::from_raw(raw) {
            BudgetedPollerState::Idle | BudgetedPollerState::Scheduled => {
                BudgetedPollerState::Scheduled
            }
            BudgetedPollerState::Running | BudgetedPollerState::RunningPending => {
                BudgetedPollerState::RunningPending
            }
        };
        (raw.wrapping_add(BudgetedPollerState::PUBLISH_GENERATION_STEP)
            & !BudgetedPollerState::MASK)
            | state as usize
    }

    fn raw_with_state(raw: usize, state: BudgetedPollerState) -> usize {
        (raw & !BudgetedPollerState::MASK) | state as usize
    }

    fn clear_scheduled_if_unchanged(&self, expected: usize) {
        let _ = self.state.compare_exchange(
            expected,
            Self::raw_with_state(expected, BudgetedPollerState::Idle),
            Ordering::AcqRel,
            Ordering::Acquire,
        );
    }

    fn assist_once(&self) {
        self.poll_scheduled_once(self.assist_budget);
    }

    fn poll_background_batch(&self, next_work: usize) {
        if !self.try_start_run() {
            return;
        }

        let mut has_more = false;
        for _ in 0..self.max_background_rounds.max(1) {
            has_more = (self.poll_once)(self.background_budget).has_more;
            if !has_more {
                break;
            }
        }

        self.finish_run(has_more, next_work);
    }

    fn poll_scheduled_once(&self, budget: B) {
        if !self.try_start_run() {
            return;
        }

        let progress = (self.poll_once)(budget);
        self.finish_run(progress.has_more, 0);
    }

    fn try_start_run(&self) -> bool {
        let mut current = self.state.load(Ordering::Acquire);
        loop {
            if BudgetedPollerState::from_raw(current) != BudgetedPollerState::Scheduled {
                return false;
            }
            match self.state.compare_exchange_weak(
                current,
                Self::raw_with_state(current, BudgetedPollerState::Running),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(observed) => current = observed,
            }
        }
    }

    fn finish_run(&self, has_more: bool, next_work: usize) {
        let mut current = self.state.load(Ordering::Acquire);
        let (next_state, next_raw) = loop {
            let current_state = BudgetedPollerState::from_raw(current);
            let next_state = match (current_state, has_more) {
                (BudgetedPollerState::Running, false) => BudgetedPollerState::Idle,
                (BudgetedPollerState::Running | BudgetedPollerState::RunningPending, true)
                | (BudgetedPollerState::RunningPending, false) => BudgetedPollerState::Scheduled,
                _ => unreachable!("budgeted poller released without ownership"),
            };
            match self.state.compare_exchange_weak(
                current,
                Self::raw_with_state(current, next_state),
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => break (next_state, Self::raw_with_state(current, next_state)),
                Err(observed) => current = observed,
            }
        };

        if next_state == BudgetedPollerState::Scheduled
            && !self.queue_background_work(next_work)
            && self.started.load(Ordering::Acquire)
        {
            self.clear_scheduled_if_unchanged(next_raw);
        }
    }

    fn queue_background_work(&self, work_index: usize) -> bool {
        let queue = self.queue.lock().clone();
        let Some(queue) = queue else {
            return false;
        };

        match queue.queue_work(&self.works[work_index]) {
            QueueWorkResult::Queued => true,
            QueueWorkResult::AlreadyQueued => {
                let alternate = work_index ^ 1;
                match queue.queue_work(&self.works[alternate]) {
                    QueueWorkResult::Queued | QueueWorkResult::AlreadyQueued => true,
                    QueueWorkResult::Disabled
                    | QueueWorkResult::QueueFull
                    | QueueWorkResult::WorkerUnavailable
                    | QueueWorkResult::InvalidCpu => false,
                }
            }
            QueueWorkResult::Disabled
            | QueueWorkResult::QueueFull
            | QueueWorkResult::WorkerUnavailable
            | QueueWorkResult::InvalidCpu => false,
        }
    }

    fn destroy(&self) -> Result<(), WorkqueueError> {
        self.is_destroying.store(true, Ordering::Release);
        let queue = self.queue.lock().take();
        if let Some(queue) = queue {
            queue.destroy()?;
        }
        self.state
            .store(BudgetedPollerState::Idle as usize, Ordering::Release);
        self.started.store(false, Ordering::Release);
        Ok(())
    }

    fn load_state(&self) -> BudgetedPollerState {
        BudgetedPollerState::from_raw(self.state.load(Ordering::Acquire))
    }

    #[cfg(unittest)]
    fn force_state_for_tests(&self, state: BudgetedPollerState) {
        self.state.store(state as usize, Ordering::Release);
    }
}

/// A NAPI-like coalesced budgeted poller.
///
/// Producers call [`Self::notify_irq_safe`] to publish work. At most one
/// executor owns the poller at a time. Notifications that arrive while the
/// poller is running are converted into a follow-up scheduled round instead of
/// creating a concurrent executor or losing the wake.
pub struct BudgetedPoller<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    inner: Arc<BudgetedPollerInner<B, F>>,
}

impl<B, F> BudgetedPoller<B, F>
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    /// Creates an unstarted poller.
    ///
    /// `max_background_rounds` bounds how many immediate rounds one worker
    /// callback may run before rescheduling a follow-up callback.
    pub fn new(
        name: &'static str,
        background_budget: B,
        assist_budget: B,
        max_background_rounds: usize,
        poll_once: F,
    ) -> Self {
        let inner = Arc::new_cyclic(|weak| BudgetedPollerInner {
            name,
            started: AtomicBool::new(false),
            is_destroying: AtomicBool::new(false),
            queue: SpinNoIrq::new(None),
            works: [work_for(weak.clone(), 1), work_for(weak.clone(), 0)],
            state: AtomicUsize::new(BudgetedPollerState::Idle as usize),
            background_budget,
            assist_budget,
            max_background_rounds,
            poll_once,
        });
        Self { inner }
    }

    /// Starts the backing task-context workerqueue lane.
    pub fn start(&self) -> Result<(), BudgetedPollerStartError> {
        self.inner.start()
    }

    /// Publishes work and queues the background poller if it was idle.
    ///
    /// This method does not run the poll callback and may be called from
    /// IRQ-adjacent producers.
    pub fn notify_irq_safe(&self) -> bool {
        self.inner.notify_irq_safe()
    }

    /// Assists one already scheduled round from the current task context.
    pub fn assist_once(&self) {
        self.inner.assist_once();
    }

    /// Gates new backing work permanently and waits for queued/running poll callbacks.
    pub fn destroy(&self) -> Result<(), WorkqueueError> {
        self.inner.destroy()
    }

    #[cfg(unittest)]
    pub(crate) fn poll_background_batch_for_tests(&self) {
        self.inner.poll_background_batch(0);
    }

    #[cfg(unittest)]
    pub(crate) fn force_running_for_tests(&self) {
        self.inner
            .force_state_for_tests(BudgetedPollerState::Running);
    }

    #[cfg(unittest)]
    pub(crate) fn force_scheduled_for_tests(&self) {
        self.inner
            .force_state_for_tests(BudgetedPollerState::Scheduled);
    }

    #[cfg(unittest)]
    pub(crate) fn finish_run_for_tests(&self, has_more: bool) {
        self.inner.finish_run(has_more, 0);
    }

    #[cfg(unittest)]
    pub(crate) fn is_idle_for_tests(&self) -> bool {
        self.inner.load_state() == BudgetedPollerState::Idle
    }

    #[cfg(unittest)]
    pub(crate) fn is_scheduled_for_tests(&self) -> bool {
        self.inner.load_state() == BudgetedPollerState::Scheduled
    }

    #[cfg(unittest)]
    pub(crate) fn is_running_pending_for_tests(&self) -> bool {
        self.inner.load_state() == BudgetedPollerState::RunningPending
    }
}

fn work_for<B, F>(inner: Weak<BudgetedPollerInner<B, F>>, next_work: usize) -> ScheduledWork
where
    B: Copy + Send + Sync + 'static,
    F: Fn(B) -> BudgetedPollProgress + Send + Sync + 'static,
{
    ScheduledWork::new(move |_work| {
        if let Some(inner) = inner.upgrade() {
            inner.poll_background_batch(next_work);
        }
    })
}

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;

    static POLL_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn poller() -> BudgetedPoller<usize, impl Fn(usize) -> BudgetedPollProgress> {
        BudgetedPoller::new("budgeted_poller_test", 8, 1, 2, |budget| {
            POLL_CALLS.fetch_add(budget, Ordering::AcqRel);
            BudgetedPollProgress { has_more: false }
        })
    }

    #[def_test(serial)]
    fn notify_before_start_keeps_work_scheduled() {
        POLL_CALLS.store(0, Ordering::Release);
        let poller = poller();

        assert_eq!(poller.notify_irq_safe(), false);
        assert!(poller.is_scheduled_for_tests());
        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 0);
    }

    #[def_test(serial)]
    fn scheduled_background_batch_runs_once_and_becomes_idle() {
        POLL_CALLS.store(0, Ordering::Release);
        let poller = poller();
        poller.force_scheduled_for_tests();

        poller.poll_background_batch_for_tests();

        assert!(poller.is_idle_for_tests());
        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 8);
    }

    #[def_test(serial)]
    fn notify_while_running_records_followup_round() {
        POLL_CALLS.store(0, Ordering::Release);
        let poller = poller();
        poller.force_running_for_tests();

        assert_eq!(poller.notify_irq_safe(), true);
        assert!(poller.is_running_pending_for_tests());
        poller.finish_run_for_tests(false);

        assert!(poller.is_scheduled_for_tests());
        assert_eq!(POLL_CALLS.load(Ordering::Acquire), 0);
    }
}
