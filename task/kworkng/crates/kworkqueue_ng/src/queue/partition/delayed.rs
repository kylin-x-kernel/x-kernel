// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Timer-to-ordinary transfer using the existing pending slot and intrusive node.

use alloc::{sync::Arc, vec::Vec};

use super::{Queue, QueuePartition, SubmitError, Work};
use crate::work::{delayed::ArmError, observers::PreparedObserver};

impl QueuePartition {
    /// Captures current Work subjects and expedites timer placement atomically.
    /// Receipts are prepared and registered before this transaction. False requests
    /// retry after a stale owner or insufficient receipt capacity, without effects.
    /// Running timer delivery also requests retry, with no receipts attached.
    /// Retry only after releasing management locks and allowing IRQ progress.
    /// Ordinary/running work keeps its placement and gates. Successful capture
    /// follows physical timer quiescence; no later timer cancellation is needed.
    /// This is not a complete delayed flush API. No new occurrence is accepted.
    ///
    /// # Safety
    /// Retain shared pinned Work and its matching bound timer through all accesses
    /// and both drains. Perform context/self-wait validation before invoking this
    /// effectful primitive. The handler obeys the non-inline IRQ delivery contract.
    ///
    /// # Errors
    /// Disabled/closed timer transfer or count overflow rejects before attaching
    /// receipts, canceling the timer or changing placement/accounting.
    ///
    /// # Panics
    /// Panics on broken list or protected capture-capacity invariants.
    pub(crate) unsafe fn capture_and_expedite<
        H: Fn() + Send + Sync,
        TH: ktask::TimerHandler + 'static,
    >(
        self: &Arc<Self>,
        work: &Work<Arc<Queue>, H>,
        timer: &ktask::Timer<TH>,
        prepared: &mut Vec<PreparedObserver>,
    ) -> Result<bool, SubmitError> {
        // SAFETY: forward the same pinned matching pair and preflight contract;
        // the provider is nonwaiting and does not call handlers under these locks.
        unsafe { self.capture_and_expedite_with(work, prepared, || timer.try_cancel()) }
    }

    /// Shared transaction; tests inject a running provider without blocking an IRQ.
    ///
    /// # Safety
    /// Same storage/preflight contract as capture_and_expedite. cancel must be the
    /// matching nonwaiting timer operation (or a controlled test model), must not
    /// unwind, allocate, notify or reenter management locks.
    ///
    /// # Errors
    /// Transfer admission and count failures precede the provider call and capture.
    ///
    /// # Panics
    /// Panics on broken list, capture capacity or accounting invariants.
    pub(crate) unsafe fn capture_and_expedite_with<H: Fn() + Send + Sync>(
        self: &Arc<Self>,
        work: &Work<Arc<Queue>, H>,
        prepared: &mut Vec<PreparedObserver>,
        cancel: impl FnOnce() -> ktask::CancelResult,
    ) -> Result<bool, SubmitError> {
        let mut partition = self.state.lock();
        #[cfg(feature = "stall-check")]
        let had_backlog = partition.has_backlog();
        let mut state = work.state.lock();
        if !state.owner_matches(Some(self)) || prepared.len() < state.observer_count() {
            return Ok(false);
        }
        if state.delayed_deadline_ns().is_none() {
            if cancel() == ktask::CancelResult::Running {
                return Ok(false);
            }
            state.capture(prepared);
            return Ok(true);
        }
        if state.is_disabled() {
            return Err(SubmitError::Disabled);
        }
        let color = state
            .pending_resources()
            .expect("validated timer placement")
            .admission_color()
            .ok_or(SubmitError::Closed)?;
        self.in_flight
            .add(color)
            .map_err(|_| SubmitError::CountOverflow)?;
        // Reserve before touching the timer: overflow must leave it armed.
        // Running leaves the physical timer untouched, so undo only this charge.
        if cancel() == ktask::CancelResult::Running {
            let observers = self.in_flight.retire_after_cleanup(color);
            drop(state);
            drop(partition);
            observers.complete();
            return Ok(false);
        }
        // Work protection excludes rearm until capture and ordinary publication.
        // A successful physical stop precedes scope establishment, never follows it.
        state.capture(prepared);
        state.queue_delayed_pending(color);
        // SAFETY: both guards validate the current timer-only owner and unlinked
        // embedded node. The caller retains pinned Work through execution/drain.
        assert!(unsafe { partition.waiting.push_back(work.pending_ptr()) });
        drop(state);
        let kick = self.release_waiting(&mut partition);
        #[cfg(feature = "stall-check")]
        partition.publish_backlog(had_backlog, false);
        drop(partition);
        if let Some(kick) = kick {
            kick.wake_by_ref();
        }
        Ok(true)
    }

    /// Reprograms current pending work, withdrawing an ordinary queueing if needed.
    /// Returns false for stale ownership or no pending target; no new work is accepted.
    /// Timer-only retime preserves observers. Ordinary withdrawal ends the old
    /// color/observer scope, not the accepted request's resources or ownership.
    /// None requests immediate ordinary queueing; existing ordinary placement
    /// keeps its FIFO position, allowance, color and observers.
    ///
    /// # Safety
    /// Keep the shared pinned Work and its matching bound timer live through all
    /// accesses and both drains. Current CPU timer service must be initialized;
    /// the handler must obey the existing non-inline IRQ delivery contract.
    ///
    /// # Errors
    /// Disabled, closed and invalid deadlines leave timer, FIFO, allowance and
    /// observer/accounting state unchanged. All fallible steps precede withdrawal.
    ///
    /// # Panics
    /// Panics on broken list/accounting or initialized timer-service invariants.
    pub(crate) unsafe fn retime_pending<
        H: Fn() + Send + Sync,
        TH: ktask::TimerHandler + 'static,
    >(
        self: &Arc<Self>,
        work: &Work<Arc<Queue>, H>,
        timer: &ktask::Timer<TH>,
        deadline: Option<ktime_types::MonotonicInstant>,
    ) -> Result<bool, ArmError> {
        let mut partition = self.state.lock();
        #[cfg(feature = "stall-check")]
        let had_backlog = partition.has_backlog();
        let mut state = work.state.lock();
        if !state.pending_on(self) {
            return Ok(false);
        }
        if state.is_disabled() {
            return Err(ArmError::Admission(SubmitError::Disabled));
        }
        let color = state
            .pending_resources()
            .expect("validated pending")
            .admission_color()
            .ok_or(ArmError::Admission(SubmitError::Closed))?;
        let Some(deadline) = deadline else {
            if state.delayed_deadline_ns().is_none() {
                return Ok(true);
            }
            self.in_flight
                .add(color)
                .map_err(|_| ArmError::Admission(SubmitError::CountOverflow))?;
            // Logical placement is authoritative even if delivery already runs.
            // Cancel under the same guards so it cannot cancel a later rearm.
            // Running may retain a physical successor; its stale delivery is
            // harmless and final cancel_sync still drains it before storage release.
            let _ = timer.try_cancel();
            state.queue_delayed_pending(color);
            // SAFETY: caller retains the pinned Work. Both guards validate timer
            // placement, hence this embedded node is not linked in either FIFO.
            assert!(unsafe { partition.waiting.push_back(work.pending_ptr()) });
            drop(state);
            let kick = self.release_waiting(&mut partition);
            #[cfg(feature = "stall-check")]
            partition.publish_backlog(had_backlog, false);
            drop(partition);
            if let Some(kick) = kick {
                kick.wake_by_ref();
            }
            return Ok(true);
        };
        let deadline_ns = deadline
            .span_since_origin()
            .try_as_nanos_u64()
            .ok_or(ArmError::Timer(kerrno::KError::InvalidInput))?;
        // SAFETY: caller retains the matching timer/Work pair and initialized
        // service. Delivery drops timer locks before taking partition -> Work.
        unsafe { timer.start(deadline) }.map_err(ArmError::Timer)?;
        if state.delayed_deadline_ns().is_some() {
            state.retime_pending(deadline_ns);
            return Ok(true);
        }
        let is_ready = state.pending_is_ready();
        let list = if is_ready {
            &mut partition.ready
        } else {
            &mut partition.waiting
        };
        // SAFETY: both guards validated this exact pending owner and ordinary
        // phase; caller retains Work and its embedded node through withdrawal.
        assert!(unsafe { list.remove(work.pending_node()) });
        let (color, observers) = state.withdraw_pending_to_timer(deadline_ns);
        drop(state);
        let kick = if is_ready {
            partition.active -= 1;
            self.release_waiting(&mut partition)
        } else {
            None
        };
        #[cfg(feature = "stall-check")]
        partition.publish_backlog(had_backlog, false);
        drop(partition);
        if let Some(kick) = kick {
            kick.wake_by_ref();
        }
        // The same resources remain needed by the new timer. Only the old
        // queueing obligation ends; no Work access follows observer notification.
        self.retire_after_cleanup(color).complete();
        observers.complete();
        Ok(true)
    }

    /// Programs a prepared timer and publishes a fresh logical acceptance together.
    ///
    /// # Safety
    /// Queue must own this partition. Retain shared stable Work and the timer's
    /// matching borrowed binding through all timer/ordinary accesses; current CPU
    /// timer service must be initialized. No business callback may run inline.
    ///
    /// # Errors
    /// All recoverable admission/range/provider failures precede placement changes.
    /// Returns false without effects if the routing owner snapshot has changed.
    pub(crate) unsafe fn arm_delayed<H: Fn() + Send + Sync, TH: ktask::TimerHandler + 'static>(
        self: &Arc<Self>,
        queue: &Arc<Queue>,
        work: &Work<Arc<Queue>, H>,
        timer: &ktask::Timer<TH>,
        deadline: ktime_types::MonotonicInstant,
        expected_owner: Option<&Arc<Self>>,
    ) -> Result<bool, ArmError> {
        let _partition = self.state.lock();
        let mut state = work.state.lock();
        if !state.owner_matches(expected_owner) {
            return Ok(false);
        }
        if state.is_disabled() {
            return Err(ArmError::Admission(SubmitError::Disabled));
        }
        if state.has_pending() {
            return Err(ArmError::Admission(SubmitError::AlreadyQueued));
        }
        if !state.can_accept_on(self) {
            return Err(ArmError::Admission(SubmitError::OwnerConflict));
        }
        if queue.admission_color().is_none() {
            return Err(ArmError::Admission(SubmitError::Closed));
        }
        let deadline_ns = deadline
            .span_since_origin()
            .try_as_nanos_u64()
            .ok_or(ArmError::Timer(kerrno::KError::InvalidInput))?;
        // SAFETY: caller supplies initialized service and a matching live binding.
        // Timer dispatch drops core locks before delivery; delivery takes partition
        // then Work, so no callback can observe a partially published acceptance.
        unsafe { timer.start(deadline) }.map_err(ArmError::Timer)?;
        state.set_delayed_pending(queue.clone(), self, deadline_ns);
        Ok(true)
    }

    /// Installs timer-only fixture state without starting a physical timer.
    ///
    /// # Panics
    /// Panics on mismatched Queue ownership or invalid pending admission.
    #[cfg(unittest)]
    pub(in crate::queue) fn install_delayed_for_test<H>(
        self: &Arc<Self>,
        queue: &Arc<Queue>,
        work: &Work<Arc<Queue>, H>,
        deadline_ns: u64,
    ) {
        assert!(
            queue
                .partition(self.cpu_id)
                .is_some_and(|p| Arc::ptr_eq(p, self))
        );
        let _partition = self.state.lock();
        work.state
            .lock()
            .set_delayed_pending(queue.clone(), self, deadline_ns);
    }

    /// Queues the current timer-side occurrence once its authoritative deadline is due.
    ///
    /// Returns false for stale ownership, no timer placement or a future deadline.
    /// True means ordinary participation was established, not that execution began.
    /// Samples current queue control under the same admission barrier as ordinary
    /// submission. It preserves payload/observers and does not accept new work.
    /// Demand publication is protected; service wake runs only after both locks end.
    /// No allocation, timer programming or inline business callback occurs here.
    ///
    /// # Safety
    /// Work and captures must remain stable, live and shared through all accepted
    /// occurrences and concurrent accesses, including any execution this publishes.
    /// now_ns must be an observation of the monotonic clock, not an old timer's
    /// expiration argument or a speculative future time.
    ///
    /// # Errors
    /// Disabled, Closed and CountOverflow leave timer placement unchanged. The
    /// integrating timer owner must handle these outcomes, not silently lose work.
    ///
    /// # Panics
    /// Panics if timer placement has a linked node. Debug builds also check that
    /// the retained Queue owns this partition; both are internal invariants.
    pub(crate) unsafe fn queue_due<H: Fn() + Send + Sync>(
        self: &Arc<Self>,
        work: &Work<Arc<Queue>, H>,
        now_ns: u64,
    ) -> Result<bool, SubmitError> {
        let mut partition = self.state.lock();
        #[cfg(feature = "stall-check")]
        let had_backlog = partition.has_backlog();
        let mut state = work.state.lock();
        if !state.pending_on(self)
            || state
                .delayed_deadline_ns()
                .is_none_or(|deadline| deadline > now_ns)
        {
            return Ok(false);
        }
        if state.is_disabled() {
            return Err(SubmitError::Disabled);
        }
        let queue = state
            .pending_resources()
            .expect("validated timer placement");
        debug_assert!(
            queue
                .partition(self.cpu_id)
                .is_some_and(|p| Arc::ptr_eq(p, self))
        );
        let color = queue.admission_color().ok_or(SubmitError::Closed)?;
        self.in_flight
            .add(color)
            .map_err(|_| SubmitError::CountOverflow)?;
        state.queue_delayed_pending(color);
        // SAFETY: caller retains the concrete Work and its stable embedded node.
        // Both guards revalidate timer-only ownership (hence no list membership)
        // and exclude concurrent cancellation, transfer and route changes.
        assert!(unsafe { partition.waiting.push_back(work.pending_ptr()) });
        drop(state);
        let kick = self.release_waiting(&mut partition);
        #[cfg(feature = "stall-check")]
        partition.publish_backlog(had_backlog, false);
        drop(partition);
        if let Some(kick) = kick {
            kick.wake_by_ref();
        }
        Ok(true)
    }
}

#[cfg(unittest)]
mod tests;

#[cfg(unittest)]
mod timer_tests;

#[cfg(unittest)]
mod retime_tests;
