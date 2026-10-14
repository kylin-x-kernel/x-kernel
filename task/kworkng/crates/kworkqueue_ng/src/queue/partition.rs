// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue-owned per-CPU admission, execution allowance and original accounting.

#[cfg(feature = "self-wait-check")]
pub(crate) mod callbacks;
mod delayed;
mod inflight;
pub(crate) mod pending;

use alloc::sync::Arc;
use core::{num::NonZeroUsize, task::Waker};

use inflight::QueueInFlight;
use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrq;
use pending::{PendingList, PendingNode};

use super::source::QueueSource;
use crate::{
    queue::{Queue, submission::SubmitError},
    work::{
        Work,
        execution::Execution,
        observers::{CompletionObservers, PreparedObserver},
        state::{WorkSlot, WorkState},
    },
};

/// One Queue's per-CPU partition, retained by accepted occurrences through cleanup.
///
/// CPU identity is immutable and lets unfinished Work route through its retained
/// partition without scanning Queue's array. Other CPUs may submit,
/// cancel or inspect it under the same locks; it is not CPU-local-only storage.
/// Contains management state, not a strong back-reference to Queue or Work.
/// Accepted occurrences retain Queue separately until resource cleanup. This
/// partition stays alive through subsequent accounting without an owning cycle.
pub(crate) struct QueuePartition {
    cpu_id: LogicalCpuId,
    state: SpinNoIrq<PartitionState>,
    in_flight: QueueInFlight,
    max_active: NonZeroUsize,
    source: Option<QueueSource>,
}

/// A changed routing snapshot requires retry, not a caller-visible rejection.
pub(super) enum AdmissionResult {
    Accepted(Option<Arc<Waker>>),
    Retry,
    Rejected(SubmitError),
}

/// Pending membership and execution allowance protected by one partition lock.
struct PartitionState {
    #[cfg(feature = "stall-check")]
    diagnostic: Option<Arc<crate::diagnostics::StallRecord>>,
    #[cfg(feature = "self-wait-check")]
    callbacks: intrusive_collections::LinkedList<callbacks::CallbackAdapter>,
    waiting: PendingList,
    ready: PendingList,
    active: usize,
}

#[cfg(feature = "stall-check")]
impl PartitionState {
    fn has_backlog(&self) -> bool {
        !self.waiting.is_empty() || !self.ready.is_empty()
    }

    /// Queue membership, not allowance or running callbacks, defines backlog.
    fn publish_backlog(&self, had_backlog: bool, has_progress: bool) {
        let Some(diagnostic) = &self.diagnostic else {
            return;
        };
        let has_backlog = self.has_backlog();
        if had_backlog == has_backlog && !(has_progress && has_backlog) {
            return;
        }
        diagnostic.publish_interval(has_backlog.then(khal::time::monotonic_time));
    }
}

impl QueuePartition {
    /// Prepares one partition with a positive concurrent-execution allowance.
    #[cfg(unittest)]
    pub(crate) fn new(cpu_id: LogicalCpuId, max_active: NonZeroUsize) -> Self {
        Self::with_source(cpu_id, max_active, None)
    }

    pub(super) fn with_source(
        cpu_id: LogicalCpuId,
        max_active: NonZeroUsize,
        source: Option<QueueSource>,
    ) -> Self {
        Self {
            cpu_id,
            state: SpinNoIrq::new(PartitionState {
                #[cfg(feature = "stall-check")]
                diagnostic: None,
                #[cfg(feature = "self-wait-check")]
                callbacks: intrusive_collections::LinkedList::new(callbacks::CallbackAdapter::NEW),
                waiting: PendingList::new(),
                ready: PendingList::new(),
                active: 0,
            }),
            in_flight: QueueInFlight::new(),
            max_active,
            source,
        }
    }

    /// Binds a prepared record before this partition can be shared or submitted to.
    /// Monitoring can retain only this diagnostic data, never the partition.
    #[cfg(feature = "stall-check")]
    pub(crate) fn with_diagnostic(
        mut self,
        diagnostic: Arc<crate::diagnostics::StallRecord>,
    ) -> Self {
        let state = self.state.get_mut();
        assert!(state.diagnostic.is_none() && !state.has_backlog() && state.active == 0);
        state.diagnostic = Some(diagnostic);
        self
    }

    pub(super) fn cpu_id(&self) -> LogicalCpuId {
        self.cpu_id
    }

    /// Obtain the shared one-shot receipt before queue closure changes state.
    pub(super) fn source_detachment_completion(&self) -> Option<Arc<kpoll::Completion>> {
        self.source.as_ref().map(QueueSource::detachment_completion)
    }

    /// Call unlocked after admission closure and ordinary work drain. The binding
    /// remains immutable so concurrent shutdown callers share the same receipt.
    pub(super) fn detach_source(&self) {
        if let Some(source) = &self.source {
            source.detach();
        }
    }

    /// Admits a Queue-owned occurrence with its final control read under protection.
    ///
    /// # Safety
    /// Keep Work stable, shared and live through all accepted occurrences and accesses.
    /// This partition must belong to queue; Queue::submit selects it from its array.
    pub(super) unsafe fn accept_from_queue<H: Fn() + Send + Sync>(
        self: &Arc<Self>,
        queue: &Arc<Queue>,
        work: &Work<Arc<Queue>, H>,
        expected_owner: Option<&Arc<Self>>,
    ) -> AdmissionResult {
        let mut partition = self.state.lock();
        #[cfg(feature = "stall-check")]
        let had_backlog = partition.has_backlog();
        let mut state = work.state.lock();
        if !state.owner_matches(expected_owner) {
            return AdmissionResult::Retry;
        }
        let Some(color) = queue.admission_color() else {
            return AdmissionResult::Rejected(SubmitError::Closed);
        };
        // SAFETY: the caller retains external Work; both guards serialize publication.
        let result =
            unsafe { self.insert_accepted(&mut partition, &mut state, work, queue.clone(), color) };
        drop(state);
        match result {
            Ok(()) => {
                let kick = self.release_waiting(&mut partition);
                #[cfg(feature = "stall-check")]
                partition.publish_backlog(had_backlog, false);
                drop(partition);
                AdmissionResult::Accepted(kick)
            }
            Err((resources, error)) => {
                drop(partition);
                drop(resources);
                AdmissionResult::Rejected(error)
            }
        }
    }

    /// Admits arbitrary resources/colors only for isolated lifecycle tests.
    ///
    /// Production admission obtains color from Queue under this partition guard;
    /// explicit colors here support isolated accounting and destructor tests.
    /// Pending membership is published in the same transaction as the Work slot.
    /// A bound partition publishes released demand and kicks after unlocking.
    /// Resources must be transferable and callback storage shareable across CPUs;
    /// these bounds do not replace the external lifetime contract.
    ///
    /// # Safety
    /// Keep Work storage alive, at the same address and accessible only through
    /// shared references until all accepted occurrences and concurrent accesses
    /// have ended. Stop producers and drain before freeing storage. Neither the
    /// pending list nor an accepted partition reference retains ownership of Work.
    ///
    /// # Errors
    /// Returns resources unchanged for occupied pending, another running owner,
    /// or count exhaustion. Resource destruction is the caller's unlocked duty.
    ///
    /// # Panics
    /// Panics if the internal color is outside the prepared ring, or an internal
    /// invariant violation left a node linked despite empty pending storage.
    #[cfg(unittest)]
    pub(crate) unsafe fn accept<T: Send, H: Send + Sync>(
        self: &Arc<Self>,
        work: &Work<T, H>,
        resources: T,
        color: usize,
    ) -> Result<(), T> {
        // SAFETY: the test caller supplies the same storage/drain obligation.
        unsafe { self.accept_using(work, resources, || Some(color)) }
    }

    /// Commits control validation, original accounting and pending publication.
    ///
    /// select_color is private management code, never a business callback. The
    /// final control sample belongs inside the partition barrier even if an
    /// earlier caller-side rejection check is added later.
    ///
    /// # Safety
    /// Work must remain stable, shared and live until every accepted occurrence
    /// and concurrent access is drained. The list does not retain Work ownership.
    #[cfg(unittest)]
    pub(in crate::queue) unsafe fn accept_using<T: Send, H: Send + Sync>(
        self: &Arc<Self>,
        work: &Work<T, H>,
        resources: T,
        select_color: impl FnOnce() -> Option<usize>,
    ) -> Result<(), T> {
        let mut partition = self.state.lock();
        #[cfg(feature = "stall-check")]
        let had_backlog = partition.has_backlog();
        let Some(color) = select_color() else {
            return Err(resources);
        };
        let mut state = work.state.lock();
        // SAFETY: caller retains stable external storage; both guards serialize publication.
        let result =
            unsafe { self.insert_accepted(&mut partition, &mut state, work, resources, color) };
        drop(state);
        let kick = result
            .is_ok()
            .then(|| self.release_waiting(&mut partition))
            .flatten();
        #[cfg(feature = "stall-check")]
        partition.publish_backlog(had_backlog, false);
        drop(partition);
        if let Some(kick) = kick {
            kick.wake_by_ref();
        }
        result.map_err(|(resources, _)| resources)
    }

    /// Commits one occurrence after routing/control validation, without cleanup.
    ///
    /// # Safety
    /// Hold this partition's guard and this Work's guard. Keep published Work
    /// stable, shared and live until all occurrences and concurrent accesses end.
    unsafe fn insert_accepted<T: Send, H: Send + Sync>(
        self: &Arc<Self>,
        partition: &mut PartitionState,
        state: &mut WorkState<T>,
        work: &Work<T, H>,
        resources: T,
        color: usize,
    ) -> Result<(), (T, SubmitError)> {
        if state.is_disabled() {
            return Err((resources, SubmitError::Disabled));
        }
        if state.has_pending() {
            return Err((resources, SubmitError::AlreadyQueued));
        }
        if !state.can_accept_on(self) {
            return Err((resources, SubmitError::OwnerConflict));
        }
        if self.in_flight.add(color).is_err() {
            return Err((resources, SubmitError::CountOverflow));
        }
        state.set_pending(resources, self, color);
        // SAFETY: both guards serialize this node across competing partition routes;
        // validated empty pending means its only link is not in another list.
        assert!(unsafe { partition.waiting.push_back(work.pending_ptr()) });
        Ok(())
    }

    /// Charges allowance when waiting work becomes ready, not when it is claimed.
    fn release_waiting(self: &Arc<Self>, partition: &mut PartitionState) -> Option<Arc<Waker>> {
        let remaining = self
            .max_active
            .get()
            .checked_sub(partition.active)
            .expect("active allowance cannot exceed max_active");
        let released =
            partition
                .waiting
                .transfer_matching(&mut partition.ready, remaining, |node| {
                    // SAFETY: admission keeps the matching Work stable and live. This
                    // partition guard protects owner, transfer and allowance accounting.
                    unsafe { PendingNode::release(node, self) }
                });
        partition.active += released;
        if released == 0 {
            return None;
        }
        self.source.as_ref().map(QueueSource::publish)
    }

    /// Observes a sealed batch after crossing this partition's admission barrier.
    ///
    /// Queue control must prevent new admissions to this color until authorized
    /// reuse. This local barrier alone does not publish the global flush cutoff.
    ///
    /// # Errors
    /// Returns the original token if the local batch is already empty.
    ///
    /// # Panics
    /// Panics if color is outside the prepared ring.
    pub(crate) fn observe_batch(
        &self,
        color: usize,
        observer: PreparedObserver,
    ) -> Result<(), PreparedObserver> {
        let _partition = self.state.lock();
        self.in_flight.observe(color, observer)
    }

    /// Retires this partition's original count after cleanup and final Work access.
    ///
    /// Complete the returned notifications only after releasing management locks.
    ///
    /// # Panics
    /// Panics on invalid color or retirement without an outstanding occurrence.
    pub(crate) fn retire_after_cleanup(&self, color: usize) -> CompletionObservers {
        self.in_flight.retire_after_cleanup(color)
    }

    /// Detaches pending only if this partition still owns the current occurrence.
    ///
    /// Revalidates the route under partition -> Work protection. None means the
    /// route is stale or pending no longer exists; the caller may look up again.
    /// Returns the whole slot without destroying resources, decrementing counts
    /// or notifying observers. All those actions belong to unlocked cleanup.
    /// before_detach is an internal nonwaiting provider action invoked only after
    /// owner validation under both guards. It must not allocate, sleep, unwind or
    /// reenter queue/Work operations; no business callback belongs here.
    ///
    /// # Panics
    /// Panics if an owned pending occurrence has lost its list membership.
    pub(crate) fn detach_pending<T, H>(
        self: &Arc<Self>,
        work: &Work<T, H>,
        before_detach: impl FnOnce(),
    ) -> Option<(WorkSlot<T>, Option<Arc<Waker>>)> {
        let mut partition = self.state.lock();
        #[cfg(feature = "stall-check")]
        let had_backlog = partition.has_backlog();
        let mut state = work.state.lock();
        if !state.pending_on(self) {
            return None;
        }
        before_detach();
        // Timer-only placement has no intrusive membership or reserved allowance.
        // This revokes logical work only; the delayed owner must also synchronize
        // physical programming/drain before it permits storage release.
        if state.delayed_deadline_ns().is_some() {
            return state.cancel_pending().map(|slot| (slot, None));
        }
        let is_ready = state.pending_is_ready();
        let list = if is_ready {
            &mut partition.ready
        } else {
            &mut partition.waiting
        };
        // SAFETY: the protected pending owner and phase prove this exact list.
        // The live Work borrow and both guards exclude release and link mutation.
        assert!(unsafe { list.remove(work.pending_node()) });
        let canceled = state
            .cancel_pending()
            .expect("validated pending occurrence");
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
        Some((canceled, kick))
    }

    /// Claims this partition's ready occurrence without charging allowance again.
    ///
    /// A stale route or unreleased occurrence has no effect. The returned execution
    /// runs only after the provider has also released its source inspection locks.
    /// This test-only entry bypasses FIFO for controlled lifecycle schedules;
    /// providers must use Queue::claim_with_demand instead.
    ///
    /// # Panics
    /// Panics if a claimed pending occurrence has lost its list membership.
    #[cfg(unittest)]
    pub(crate) fn claim<'a, T: Send, H: Fn() + Send + Sync>(
        self: &Arc<Self>,
        work: &'a Work<T, H>,
    ) -> Option<crate::work::execution::BorrowedExecution<'a>> {
        let mut partition = self.state.lock();
        let mut state = work.state.lock();
        if !state.pending_on(self) || !state.claim() {
            return None;
        }
        // SAFETY: pending belonged to this partition before the protected claim;
        // both guards remain held and Work storage is borrowed for execution.
        assert!(unsafe { partition.ready.remove(work.pending_node()) });
        drop(state);
        #[cfg(feature = "stall-check")]
        partition.publish_backlog(true, true);
        drop(partition);
        Some(Execution::borrowed(work))
    }

    /// Claims the earliest released Work; blocked successors remain waiting.
    ///
    /// The returned execution is already running, with allowance charged and its
    /// node removed. Run it after ending provider source inspection. No callback,
    /// payload destruction or notification occurs under the partition lock.
    /// Returns None when no released Work is available.
    ///
    /// # Panics
    /// Panics if a linked Work's protected owner differs from this partition.
    #[cfg(unittest)]
    pub(crate) fn claim_next(self: &Arc<Self>) -> Option<Execution> {
        self.claim_with_demand().0
    }

    /// Claims once and observes remaining executable demand under the same guard.
    ///
    /// Demand means another released Work is available, not unused allowance.
    /// Later submission/retirement publishes fresh demand through release_waiting;
    /// this snapshot alone is not a retained wakeup protocol.
    ///
    /// # Panics
    /// Panics if a linked Work's protected owner differs from this partition.
    pub(super) fn claim_with_demand(self: &Arc<Self>) -> (Option<Execution>, bool) {
        let mut partition = self.state.lock();
        let Some(node) = partition.ready.pop_front() else {
            return (None, false);
        };
        // SAFETY: admission preserves concrete Work storage. The partition guard
        // protects this ready occurrence until claim; allowance was charged at release.
        let execution = unsafe { PendingNode::claim(node, self) }
            .expect("released Work remains claimable under its partition lock");
        let has_demand = !partition.ready.is_empty();
        #[cfg(feature = "stall-check")]
        partition.publish_backlog(true, true);
        drop(partition);
        (Some(execution), has_demand)
    }

    /// Clears running and returns allowance together after unlocked payload cleanup.
    ///
    /// # Panics
    /// Panics if internal execution ownership or cleanup ordering was violated.
    pub(crate) fn finish<T, H>(
        self: &Arc<Self>,
        work: &Work<T, H>,
    ) -> (WorkSlot<Option<T>>, Option<Arc<Waker>>) {
        let mut partition = self.state.lock();
        let mut state = work.state.lock();
        assert!(state.running_on(self), "running belongs to this partition");
        let finished = state
            .finish_after_cleanup()
            .expect("execution resources released before partition finish");
        drop(state);
        partition.active = partition
            .active
            .checked_sub(1)
            .expect("claimed execution holds partition allowance");
        let kick = self.release_waiting(&mut partition);
        (finished, kick)
    }
}

unittest::path_mod_test!(
    partition_unittest,
    "../../ut-tests/queue_partition_unittest.rs"
);
