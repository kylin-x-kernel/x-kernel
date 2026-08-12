// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Minimal KIRQ-owned workerqueue foundation.
//!
//! Workerqueue callbacks run in task context through a scheduler provider.
//! This module owns work state and queue ordering; it does not own task
//! creation or blocking.

use alloc::{boxed::Box, sync::Arc};

use kpoll::Completion;
use kspin::SpinNoIrq;

/// Scheduler-facing wake bridge for the system workerqueue.
///
/// `kirq` owns `system_wq` state and decides when a worker should be woken.
/// The task layer owns the actual kworker task and wait primitive.
#[kiface::interface]
pub trait WorkerqueueHostIf {
    /// Wakes the worker draining [`system_wq`].
    ///
    /// Implementations must be callable from hardirq, serving-softirq, and
    /// BH-disabled context.
    fn wake_system_worker();
}

/// Scheduler-facing current-task context bridge for worker callbacks.
///
/// Workerqueue callbacks are sleepable and may yield. KIRQ therefore records
/// the currently executing work through scheduler-owned task-local state rather
/// than through per-CPU state that could be invalidated by task migration.
#[kiface::interface]
pub trait WorkerqueueTaskContextIf {
    /// Replaces the current task's workerqueue callback context and returns
    /// the previous context, if one existed.
    fn set_current_work_context(context: WorkerqueueTaskContext) -> Option<WorkerqueueTaskContext>;

    /// Clears the current task's workerqueue callback context if it still
    /// matches `context`.
    fn clear_current_work_context(context: WorkerqueueTaskContext) -> bool;

    /// Returns the current task's workerqueue callback context, if one is
    /// executing.
    fn current_work_context() -> Option<WorkerqueueTaskContext>;
}

/// Opaque workerqueue callback identity stored in scheduler task-local state.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct WorkerqueueTaskContext {
    work_key: usize,
    queue_key: usize,
}

impl WorkerqueueTaskContext {
    /// Creates a task-local workerqueue callback context from opaque keys.
    pub const fn new(work_key: usize, queue_key: usize) -> Self {
        Self {
            work_key,
            queue_key,
        }
    }

    /// Returns the opaque work identity.
    pub const fn work_key(self) -> usize {
        self.work_key
    }

    /// Returns the opaque queue identity.
    pub const fn queue_key(self) -> usize {
        self.queue_key
    }
}

/// Scheduler-facing wait bridge for workerqueue synchronization.
///
/// `kirq` owns the work lifecycle predicate, but it does not own task
/// blocking. The scheduler layer provides this interface so sleepable
/// workerqueue APIs can block without adding a `kirq -> ktask` dependency.
#[kiface::interface]
pub trait WorkqueueSyncWaitIf {
    /// Waits until the workerqueue completion wake source is observed.
    ///
    /// The completion is only a wake source. Implementations should follow the
    /// `try_wait/register/try_wait` protocol, and callers must recheck their
    /// real predicate after this method returns.
    fn wait_for_completion(completion: &kpoll::Completion) -> Result<(), kpoll::PollRegisterError>;
}

/// Maximum number of pending work items in one fixed workerqueue.
///
/// The first workerqueue milestone keeps enqueue IRQ-safe by using a bounded
/// preallocated queue instead of allocating in `queue_work()`.
pub const MAX_WORKQUEUE_PENDING: usize = 64;

/// Workerqueue callback.
///
/// The callback runs in sleepable task context after the worker has dropped all
/// KIRQ workerqueue locks.
type WorkFunc = dyn Fn(&WorkItem) + Send + Sync + 'static;

/// Result of a non-blocking queue attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueWorkResult {
    /// The work was queued for execution.
    Queued,
    /// The work already has a queued instance.
    AlreadyQueued,
    /// A synchronous cancel is waiting for the running callback to exit.
    Disabled,
    /// The fixed queue has no free slot.
    QueueFull,
}

/// Result of a non-waiting cancel attempt.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CancelWorkResult {
    /// A pending queued instance was removed.
    CancelledPending,
    /// No queued instance existed.
    NotPending,
    /// The callback is currently running.
    Running,
}

/// Errors returned by sleepable workerqueue APIs.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkqueueError {
    /// The caller is in hardirq, serving-softirq, or BH-disabled context.
    InvalidContext,
    /// The current worker tried to wait on work that cannot make progress.
    ///
    /// In this single-consumer milestone this covers direct self-wait, waiting
    /// on pending work owned by the same queue, and conservative diagnostics for
    /// running work waited on from another worker callback.
    SelfWait,
    /// The scheduler wait provider failed.
    WaitFailed,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkStatus {
    Idle,
    Pending,
    Running,
    RunningAndQueued,
    Canceling,
}

struct WorkState {
    status: WorkStatus,
    pending_queue: Option<&'static WorkQueue>,
}

impl WorkState {
    const fn new() -> Self {
        Self {
            status: WorkStatus::Idle,
            pending_queue: None,
        }
    }

    fn pending_queue_is(&self, queue: &'static WorkQueue) -> bool {
        self.pending_queue
            .is_some_and(|pending_queue| core::ptr::eq(pending_queue, queue))
    }

    fn has_runnable_entry_on(&self, queue: &'static WorkQueue) -> bool {
        self.status == WorkStatus::Pending && self.pending_queue_is(queue)
    }
}

struct Work {
    func: Box<WorkFunc>,
    state: SpinNoIrq<WorkState>,
    done: Completion,
}

impl Work {
    fn new(func: impl Fn(&WorkItem) + Send + Sync + 'static) -> Self {
        let done = Completion::new();
        done.complete_all();
        Self {
            func: Box::new(func),
            state: SpinNoIrq::new(WorkState::new()),
            done,
        }
    }
}

/// A refcounted handle to a workerqueue item.
///
/// Cloning this handle does not allocate. Queue entries and running callbacks
/// hold their own handles, so the underlying work state remains alive until it
/// is neither queued nor running. Owners that need deterministic teardown should
/// call [`cancel_work_sync`] or [`flush_work`] before dropping their last handle.
#[derive(Clone)]
pub struct WorkItem {
    inner: Arc<Work>,
}

impl WorkItem {
    /// Creates an idle work item.
    ///
    /// This may allocate through [`Completion`]'s wake source and the callback
    /// box, so it is not an IRQ-path initializer. The enqueue path itself does
    /// not allocate.
    pub fn new(func: impl Fn(&WorkItem) + Send + Sync + 'static) -> Self {
        Self {
            inner: Arc::new(Work::new(func)),
        }
    }

    fn work(&self) -> &Work {
        &self.inner
    }

    fn run(&self) {
        (self.inner.func)(self);
    }

    fn key(&self) -> usize {
        Arc::as_ptr(&self.inner) as usize
    }

    fn same_work(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.inner, &other.inner)
    }
}

/// A fixed-capacity KIRQ workerqueue.
///
/// This milestone models a single-consumer queue. Worker providers must
/// serialize calls to [`run_one_work`] for the same queue until the later
/// multi-worker execution model defines stronger concurrent-drain semantics.
pub struct WorkQueue {
    name: &'static str,
    queue: SpinNoIrq<WorkQueueInner>,
}

impl WorkQueue {
    /// Creates an empty fixed workerqueue.
    pub const fn new(name: &'static str) -> Self {
        Self {
            name,
            queue: SpinNoIrq::new(WorkQueueInner::new()),
        }
    }

    /// Returns the queue name.
    pub const fn name(&self) -> &'static str {
        self.name
    }

    fn key(&self) -> usize {
        self as *const Self as usize
    }
}

struct WorkQueueInner {
    entries: [Option<WorkItem>; MAX_WORKQUEUE_PENDING],
    head: usize,
    len: usize,
}

impl WorkQueueInner {
    const fn new() -> Self {
        Self {
            entries: [const { None }; MAX_WORKQUEUE_PENDING],
            head: 0,
            len: 0,
        }
    }

    fn physical_index(&self, logical_index: usize) -> usize {
        (self.head + logical_index) % MAX_WORKQUEUE_PENDING
    }

    fn push(&mut self, work: &WorkItem) -> Result<(), ()> {
        if self.len == MAX_WORKQUEUE_PENDING {
            return Err(());
        }
        let tail = self.physical_index(self.len);
        self.entries[tail] = Some(work.clone());
        self.len += 1;
        Ok(())
    }

    fn pop(&mut self) -> Option<WorkItem> {
        if self.len == 0 {
            return None;
        }
        let work = self.entries[self.head].take();
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        } else {
            self.head = self.physical_index(1);
        }
        work
    }

    fn remove(&mut self, work: &WorkItem) -> Option<WorkItem> {
        let logical_index = (0..self.len).find(|index| {
            self.entries[self.physical_index(*index)]
                .as_ref()
                .is_some_and(|queued| queued.same_work(work))
        })?;
        let physical_index = self.physical_index(logical_index);
        let removed = self.entries[physical_index].take();

        for logical in logical_index..(self.len - 1) {
            let current = self.physical_index(logical);
            let next = self.physical_index(logical + 1);
            self.entries[current] = self.entries[next].take();
        }
        let tail = self.physical_index(self.len - 1);
        self.entries[tail] = None;
        self.len -= 1;
        if self.len == 0 {
            self.head = 0;
        }
        removed
    }

    fn has_runnable_work(&self, queue: &'static WorkQueue) -> bool {
        (0..self.len).any(|index| {
            self.entries[self.physical_index(index)]
                .as_ref()
                .is_some_and(|work| work.work().state.lock().has_runnable_entry_on(queue))
        })
    }

    #[cfg(unittest)]
    fn len(&self) -> usize {
        self.len
    }
}

enum PendingCancel {
    Done(CancelWorkResult, Option<WorkItem>, Option<kpoll::PollSet>),
    Retry,
}

fn cancel_pending_from_owner_queue(
    queue: &'static WorkQueue,
    work: &WorkItem,
    wait_running: bool,
) -> PendingCancel {
    let mut queue_state = queue.queue.lock();
    let mut work_state = work.work().state.lock();

    match work_state.status {
        WorkStatus::Pending if work_state.pending_queue_is(queue) => {
            let removed = queue_state.remove(work);
            if removed.is_some() {
                work_state.status = WorkStatus::Idle;
                work_state.pending_queue = None;
                let waiters = work.work().done.complete_all_defer_wake();
                PendingCancel::Done(CancelWorkResult::CancelledPending, removed, Some(waiters))
            } else {
                warn!(
                    "pending work not found in owner workerqueue {} during cancel",
                    queue.name()
                );
                work_state.status = WorkStatus::Idle;
                work_state.pending_queue = None;
                let waiters = work.work().done.complete_all_defer_wake();
                PendingCancel::Done(CancelWorkResult::NotPending, None, Some(waiters))
            }
        }
        WorkStatus::RunningAndQueued if work_state.pending_queue_is(queue) => {
            let removed = queue_state.remove(work);
            if removed.is_some() {
                if wait_running {
                    work_state.status = WorkStatus::Canceling;
                    work_state.pending_queue = None;
                    PendingCancel::Done(CancelWorkResult::Running, removed, None)
                } else {
                    work_state.status = WorkStatus::Running;
                    work_state.pending_queue = None;
                    PendingCancel::Done(CancelWorkResult::CancelledPending, removed, None)
                }
            } else {
                warn!(
                    "queued follow-up not found in owner workerqueue {} during cancel",
                    queue.name()
                );
                if wait_running {
                    work_state.status = WorkStatus::Canceling;
                    work_state.pending_queue = None;
                    PendingCancel::Done(CancelWorkResult::Running, None, None)
                } else {
                    work_state.status = WorkStatus::Running;
                    work_state.pending_queue = None;
                    PendingCancel::Done(CancelWorkResult::NotPending, None, None)
                }
            }
        }
        WorkStatus::Pending | WorkStatus::RunningAndQueued => PendingCancel::Retry,
        WorkStatus::Running | WorkStatus::Canceling => {
            PendingCancel::Done(CancelWorkResult::Running, None, None)
        }
        WorkStatus::Idle => PendingCancel::Done(CancelWorkResult::NotPending, None, None),
    }
}

static SYSTEM_WORKQUEUE: WorkQueue = WorkQueue::new("system_wq");

/// Returns the first system workerqueue.
pub fn system_wq() -> &'static WorkQueue {
    &SYSTEM_WORKQUEUE
}

/// Queues work on the first system workerqueue.
pub fn schedule_work(work: &WorkItem) -> QueueWorkResult {
    queue_work(system_wq(), work)
}

/// Queues a work item without blocking.
///
/// This function may be called from hardirq, serving-softirq, BH-disabled, or
/// task context. It does not allocate and does not execute the callback.
/// `system_wq` is provider-backed; other queues must be drained explicitly by
/// their owner in this milestone.
pub fn queue_work(queue: &'static WorkQueue, work: &WorkItem) -> QueueWorkResult {
    let (result, should_wake_system_worker) = {
        let is_system_wq = core::ptr::eq(queue, system_wq());
        let mut queue_state = queue.queue.lock();
        let had_runnable_work = is_system_wq && queue_state.has_runnable_work(queue);
        let mut work_state = work.work().state.lock();
        let mut queued_runnable_work = false;

        let result = match work_state.status {
            WorkStatus::Idle => {
                if queue_state.push(work).is_err() {
                    return QueueWorkResult::QueueFull;
                }
                work.work().done.reinit();
                work_state.status = WorkStatus::Pending;
                work_state.pending_queue = Some(queue);
                queued_runnable_work = true;
                QueueWorkResult::Queued
            }
            WorkStatus::Pending | WorkStatus::RunningAndQueued => QueueWorkResult::AlreadyQueued,
            WorkStatus::Running => {
                if queue_state.push(work).is_err() {
                    return QueueWorkResult::QueueFull;
                }
                work_state.status = WorkStatus::RunningAndQueued;
                work_state.pending_queue = Some(queue);
                QueueWorkResult::Queued
            }
            WorkStatus::Canceling => QueueWorkResult::Disabled,
        };
        (
            result,
            result == QueueWorkResult::Queued
                && queued_runnable_work
                && !had_runnable_work
                && is_system_wq,
        )
    };

    if should_wake_system_worker {
        WorkerqueueHostIf::wake_system_worker();
    }

    result
}

/// Cancels a queued instance without waiting for a running callback.
///
/// KIRQ derives the owner queue from the work state, so callers cannot turn a
/// wrong queue pointer into a false "not pending" teardown result.
pub fn cancel_work(work: &WorkItem) -> CancelWorkResult {
    let (result, removed, waiters) = loop {
        let pending_queue = {
            let work_state = work.work().state.lock();
            match work_state.status {
                WorkStatus::Pending | WorkStatus::RunningAndQueued => work_state.pending_queue,
                WorkStatus::Running | WorkStatus::Canceling => {
                    break (CancelWorkResult::Running, None, None);
                }
                WorkStatus::Idle => break (CancelWorkResult::NotPending, None, None),
            }
        };

        let Some(queue) = pending_queue else {
            warn!("queued work has no owner queue during cancel");
            break (CancelWorkResult::NotPending, None, None);
        };

        match cancel_pending_from_owner_queue(queue, work, false) {
            PendingCancel::Done(result, removed, waiters) => break (result, removed, waiters),
            PendingCancel::Retry => continue,
        }
    };

    drop(removed);
    if let Some(waiters) = waiters {
        let _ = waiters.wake();
    }
    result
}

/// Waits until `work` is idle.
///
/// Returns `Ok(true)` when the work was pending, running, or already under
/// synchronous cancellation at the time of observation. Returns `Ok(false)` for
/// idle work. This function may sleep and rejects hardirq, active softirq, and
/// BH-disabled contexts. Callers that require Linux-style teardown semantics
/// must prevent unrelated threads from requeueing the same work while flushing.
/// In this milestone's single-consumer model, a worker callback also cannot
/// flush another work item that is still pending on the same queue; that case
/// returns [`WorkqueueError::SelfWait`]. The same conservative diagnostic is
/// returned when a worker callback tries to flush a running work item, because
/// that work can be requeued onto the current single-consumer queue before it
/// reaches idle.
pub fn flush_work(work: &WorkItem) -> Result<bool, WorkqueueError> {
    reject_invalid_wait_context()?;
    reject_self_wait(work)?;
    let worker_context = WorkerqueueTaskContextIf::current_work_context();

    let observed = {
        let work_state = work.work().state.lock();
        reject_worker_wait_deadlock(&work_state, worker_context)?;
        work_state.status
    };
    if observed == WorkStatus::Idle {
        return Ok(false);
    }

    wait_for_work_idle(work, worker_context)?;
    Ok(true)
}

/// Cancels pending work and waits for a running callback to finish.
///
/// KIRQ derives and removes the pending owner queue from `work` itself. The
/// caller is responsible for preventing unrelated threads from queueing the work
/// after this function returns, matching Linux-style teardown ownership. This
/// function may sleep and rejects hardirq, active softirq, and BH-disabled
/// contexts.
pub fn cancel_work_sync(work: &WorkItem) -> Result<bool, WorkqueueError> {
    reject_invalid_wait_context()?;
    reject_self_wait(work)?;

    let (must_wait, removed, waiters) = loop {
        let pending_queue = {
            let mut work_state = work.work().state.lock();
            match work_state.status {
                WorkStatus::Idle => return Ok(false),
                WorkStatus::Running => {
                    work_state.status = WorkStatus::Canceling;
                    break (true, None, None);
                }
                WorkStatus::Canceling => break (true, None, None),
                WorkStatus::Pending | WorkStatus::RunningAndQueued => work_state.pending_queue,
            }
        };

        let Some(queue) = pending_queue else {
            warn!("queued work has no owner queue during cancel_sync");
            return Ok(false);
        };

        match cancel_pending_from_owner_queue(queue, work, true) {
            PendingCancel::Done(result, removed, waiters) => {
                break (result == CancelWorkResult::Running, removed, waiters);
            }
            PendingCancel::Retry => continue,
        }
    };

    drop(removed);
    if let Some(waiters) = waiters {
        let _ = waiters.wake();
    }

    if must_wait {
        wait_for_work_idle(work, WorkerqueueTaskContextIf::current_work_context())?;
    }
    Ok(true)
}

/// Runs one pending work item from `queue`.
///
/// This is the worker provider entry point. It must be called from sleepable
/// task context. The callback runs with all KIRQ workerqueue locks released.
/// Calls for the same queue must be serialized by the worker provider in this
/// milestone. M4 does not support nested workerqueue draining from a running
/// work callback; nested calls return `false` without consuming pending work.
pub fn run_one_work(queue: &'static WorkQueue) -> bool {
    if WorkerqueueTaskContextIf::current_work_context().is_some() {
        warn!(
            "workerqueue {} rejected nested run_one_work from callback",
            queue.name()
        );
        return false;
    }

    let Some(work) = take_runnable_work(queue) else {
        return false;
    };

    {
        let _current = CurrentWorkGuard::enter(queue, &work);
        work.run();
    }
    finish_work(queue, &work);
    true
}

/// Returns whether the provider-backed system queue has runnable work.
///
/// Entries that are queued only for a follow-up run after a currently running
/// callback finishes are not runnable yet and do not wake the system worker.
pub fn system_wq_has_runnable_work() -> bool {
    system_wq().queue.lock().has_runnable_work(system_wq())
}

fn take_runnable_work(queue: &'static WorkQueue) -> Option<WorkItem> {
    let mut queue_state = queue.queue.lock();
    let attempts = queue_state.len;

    for _ in 0..attempts {
        let work = queue_state.pop()?;

        enum TakeDecision {
            Run,
            Requeue,
            DropStale(WorkStatus),
        }

        let decision = {
            let mut work_state = work.work().state.lock();
            match work_state.status {
                WorkStatus::Pending if work_state.pending_queue_is(queue) => {
                    work_state.status = WorkStatus::Running;
                    work_state.pending_queue = None;
                    TakeDecision::Run
                }
                WorkStatus::RunningAndQueued if work_state.pending_queue_is(queue) => {
                    TakeDecision::Requeue
                }
                other => TakeDecision::DropStale(other),
            }
        };

        match decision {
            TakeDecision::Run => return Some(work),
            TakeDecision::Requeue => {
                let _ = queue_state.push(&work);
            }
            TakeDecision::DropStale(status) => {
                warn!(
                    "workerqueue {} dropped stale entry with state {:?}",
                    queue.name(),
                    status
                );
            }
        }
    }

    None
}

fn finish_work(queue: &'static WorkQueue, work: &WorkItem) {
    let (waiters, followup_queue) = {
        let mut work_state = work.work().state.lock();
        match work_state.status {
            WorkStatus::Running => {
                work_state.status = WorkStatus::Idle;
                work_state.pending_queue = None;
                (Some(work.work().done.complete_all_defer_wake()), None)
            }
            WorkStatus::RunningAndQueued => {
                work_state.status = WorkStatus::Pending;
                (None, work_state.pending_queue)
            }
            WorkStatus::Canceling => {
                work_state.status = WorkStatus::Idle;
                work_state.pending_queue = None;
                (Some(work.work().done.complete_all_defer_wake()), None)
            }
            other => {
                warn!(
                    "workerqueue {} finished work with unexpected state {:?}",
                    queue.name(),
                    other
                );
                (None, None)
            }
        }
    };

    if let Some(waiters) = waiters {
        let _ = waiters.wake();
    }
    if followup_queue.is_some_and(|queue| core::ptr::eq(queue, system_wq())) {
        WorkerqueueHostIf::wake_system_worker();
    }
}

fn wait_for_work_idle(
    work: &WorkItem,
    worker_context: Option<WorkerqueueTaskContext>,
) -> Result<(), WorkqueueError> {
    loop {
        {
            let work_state = work.work().state.lock();
            if work_state.status == WorkStatus::Idle {
                return Ok(());
            }
            reject_worker_wait_deadlock(&work_state, worker_context)?;
        }
        WorkqueueSyncWaitIf::wait_for_completion(&work.work().done)
            .map_err(|_| WorkqueueError::WaitFailed)?;
    }
}

fn reject_invalid_wait_context() -> Result<(), WorkqueueError> {
    if crate::context::is_in_interrupt_context() {
        return Err(WorkqueueError::InvalidContext);
    }
    Ok(())
}

fn reject_self_wait(work: &WorkItem) -> Result<(), WorkqueueError> {
    if WorkerqueueTaskContextIf::current_work_context()
        .is_some_and(|context| context.work_key == work.key())
    {
        return Err(WorkqueueError::SelfWait);
    }
    Ok(())
}

fn reject_worker_wait_deadlock(
    work_state: &WorkState,
    worker_context: Option<WorkerqueueTaskContext>,
) -> Result<(), WorkqueueError> {
    let Some(context) = worker_context else {
        return Ok(());
    };

    let waits_on_current_queue = matches!(
        work_state.status,
        WorkStatus::Pending | WorkStatus::RunningAndQueued
    ) && work_state
        .pending_queue
        .is_some_and(|queue| queue.key() == context.queue_key);
    let waits_on_requeueable_running_work = work_state.status == WorkStatus::Running;
    if waits_on_current_queue || waits_on_requeueable_running_work {
        return Err(WorkqueueError::SelfWait);
    }
    Ok(())
}

struct CurrentWorkGuard {
    context: WorkerqueueTaskContext,
    previous_context: Option<WorkerqueueTaskContext>,
}

impl CurrentWorkGuard {
    fn enter(queue: &'static WorkQueue, work: &WorkItem) -> Self {
        let context = WorkerqueueTaskContext::new(work.key(), queue.key());
        let previous_context = WorkerqueueTaskContextIf::set_current_work_context(context);
        if previous_context.is_some() {
            warn!("nested workerqueue callback entered");
        }
        Self {
            context,
            previous_context,
        }
    }
}

impl Drop for CurrentWorkGuard {
    fn drop(&mut self) {
        if WorkerqueueTaskContextIf::current_work_context() == Some(self.context) {
            if let Some(previous_context) = self.previous_context {
                let replaced = WorkerqueueTaskContextIf::set_current_work_context(previous_context);
                if replaced != Some(self.context) {
                    warn!("workerqueue current-work state changed during callback");
                }
            } else if !WorkerqueueTaskContextIf::clear_current_work_context(self.context) {
                warn!("workerqueue current-work state changed during callback");
            }
        } else {
            warn!("workerqueue current-work state changed during callback");
        }
    }
}

/// Returns pending queue length for tests.
#[cfg(unittest)]
fn pending_len_for_tests(queue: &'static WorkQueue) -> usize {
    queue.queue.lock().len()
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::{boxed::Box, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        context::{HardIrqContextGuard, local_bh_disable},
        softirq::{SoftirqVec, raise_softirq, test_support::ScopedSoftirqAction},
    };

    static WORK_RUNS: AtomicUsize = AtomicUsize::new(0);
    static WORK_OBSERVED: AtomicUsize = AtomicUsize::new(0);
    static STORE_BEFORE_QUEUE: AtomicUsize = AtomicUsize::new(0);
    static SELF_WAIT_RESULT: AtomicUsize = AtomicUsize::new(0);
    static NESTED_RUN_RESULT: AtomicUsize = AtomicUsize::new(0);
    static REQUEUE_TEST_QUEUE: SpinNoIrq<Option<&'static WorkQueue>> = SpinNoIrq::new(None);
    static CROSS_REQUEUE_TEST_QUEUE: SpinNoIrq<Option<&'static WorkQueue>> = SpinNoIrq::new(None);
    static SOFTIRQ_TEST_QUEUE: SpinNoIrq<Option<&'static WorkQueue>> = SpinNoIrq::new(None);
    static NESTED_TEST_QUEUE: SpinNoIrq<Option<&'static WorkQueue>> = SpinNoIrq::new(None);
    static SAME_QUEUE_WAIT_TARGET: SpinNoIrq<Option<WorkItem>> = SpinNoIrq::new(None);
    static SAME_QUEUE_WAIT_RESULT: AtomicUsize = AtomicUsize::new(0);

    struct ScopedTestQueue {
        slot: &'static SpinNoIrq<Option<&'static WorkQueue>>,
        previous: Option<&'static WorkQueue>,
    }

    impl ScopedTestQueue {
        fn install(
            slot: &'static SpinNoIrq<Option<&'static WorkQueue>>,
            queue: &'static WorkQueue,
        ) -> Self {
            let previous = slot.lock().replace(queue);
            Self { slot, previous }
        }
    }

    impl Drop for ScopedTestQueue {
        fn drop(&mut self) {
            *self.slot.lock() = self.previous;
        }
    }

    fn count_work(_work: &WorkItem) {
        WORK_RUNS.fetch_add(1, Ordering::Relaxed);
    }

    fn observe_store_work(_work: &WorkItem) {
        WORK_OBSERVED.store(
            STORE_BEFORE_QUEUE.load(Ordering::Acquire),
            Ordering::Release,
        );
    }

    fn self_flush_work(work: &WorkItem) {
        let result = flush_work(work);
        let code = match result {
            Err(WorkqueueError::SelfWait) => 2,
            Ok(_) => 1,
            Err(_) => 3,
        };
        SELF_WAIT_RESULT.store(code, Ordering::Release);
    }

    fn requeue_once_work(work: &WorkItem) {
        let runs = WORK_RUNS.fetch_add(1, Ordering::Relaxed);
        let queue = REQUEUE_TEST_QUEUE
            .lock()
            .expect("requeue test queue should be installed");
        if runs == 0 && queue_work(queue, work) != QueueWorkResult::Queued {
            panic!("running work should be able to request a follow-up run");
        }
    }

    fn cross_requeue_once_work(work: &WorkItem) {
        let runs = WORK_RUNS.fetch_add(1, Ordering::Relaxed);
        let queue = CROSS_REQUEUE_TEST_QUEUE
            .lock()
            .expect("cross-requeue test queue should be installed");
        if runs == 0 && queue_work(queue, work) != QueueWorkResult::Queued {
            panic!("running work should be able to request a cross-queue follow-up run");
        }
    }

    fn nested_run_work(_work: &WorkItem) {
        let queue = NESTED_TEST_QUEUE
            .lock()
            .expect("nested test queue should be installed");
        let ran_nested = usize::from(run_one_work(queue));
        NESTED_RUN_RESULT.store(ran_nested, Ordering::Release);
    }

    fn flush_other_pending_work(_work: &WorkItem) {
        let target = SAME_QUEUE_WAIT_TARGET
            .lock()
            .as_ref()
            .cloned()
            .expect("same-queue wait target should be installed");
        let code = match flush_work(&target) {
            Err(WorkqueueError::SelfWait) => 2,
            Ok(_) => 1,
            Err(_) => 3,
        };
        SAME_QUEUE_WAIT_RESULT.store(code, Ordering::Release);
    }

    fn queue_from_softirq_action() {
        let queue = SOFTIRQ_TEST_QUEUE
            .lock()
            .expect("softirq test queue should be installed");
        let work = WorkItem::new(count_work);
        if queue_work(queue, &work) != QueueWorkResult::Queued {
            panic!("softirq action should be able to queue work");
        }
    }

    fn test_work(func: fn(&WorkItem)) -> WorkItem {
        WorkItem::new(func)
    }

    #[def_test(serial)]
    fn test_queue_work_suppresses_duplicate_pending_work() {
        let queue = WorkQueue::new("test");
        let queue = Box::leak(Box::new(queue));
        let work = test_work(count_work);

        WORK_RUNS.store(0, Ordering::Relaxed);
        assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        assert_eq!(queue_work(queue, &work), QueueWorkResult::AlreadyQueued);
        assert_eq!(pending_len_for_tests(queue), 1);

        assert!(run_one_work(queue));
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
        assert_eq!(pending_len_for_tests(queue), 0);
    }

    #[def_test(serial)]
    fn test_work_can_be_queued_again_after_idle() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = test_work(count_work);

        WORK_RUNS.store(0, Ordering::Relaxed);
        assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        assert!(run_one_work(queue));
        assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        assert!(run_one_work(queue));
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 2);
    }

    #[def_test(serial)]
    fn test_queued_work_survives_owner_handle_drop() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));

        WORK_RUNS.store(0, Ordering::Relaxed);
        {
            let work = test_work(count_work);
            assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        }

        assert!(run_one_work(queue));
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
    }

    #[def_test(serial)]
    fn test_queue_work_publishes_prior_store_to_callback() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = test_work(observe_store_work);

        STORE_BEFORE_QUEUE.store(42, Ordering::Release);
        assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        assert!(run_one_work(queue));
        assert_eq!(WORK_OBSERVED.load(Ordering::Acquire), 42);
    }

    #[def_test(serial)]
    fn test_queue_work_reports_queue_full_for_idle_work() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let mut works = Vec::new();

        for _ in 0..MAX_WORKQUEUE_PENDING {
            let work = test_work(count_work);
            assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
            works.push(work);
        }

        let overflow = test_work(count_work);
        assert_eq!(queue_work(queue, &overflow), QueueWorkResult::QueueFull);
        assert_eq!(pending_len_for_tests(queue), MAX_WORKQUEUE_PENDING);
    }

    #[def_test(serial)]
    fn test_queue_work_reports_queue_full_for_running_requeue() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let mut works = Vec::new();

        for _ in 0..MAX_WORKQUEUE_PENDING {
            let work = test_work(count_work);
            assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
            works.push(work);
        }

        let running = test_work(count_work);
        running.work().state.lock().status = WorkStatus::Running;
        assert_eq!(queue_work(queue, &running), QueueWorkResult::QueueFull);
        assert_eq!(running.work().state.lock().status, WorkStatus::Running);
    }

    #[def_test(serial)]
    fn test_workqueue_ring_buffer_preserves_fifo_after_wrap() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let mut works = Vec::new();

        WORK_OBSERVED.store(0, Ordering::Relaxed);
        STORE_BEFORE_QUEUE.store(0, Ordering::Release);

        for expected in 0..MAX_WORKQUEUE_PENDING {
            let work = WorkItem::new(move |_work| {
                let observed = WORK_OBSERVED.fetch_add(1, Ordering::Relaxed);
                if observed != expected {
                    STORE_BEFORE_QUEUE.store(expected + 1, Ordering::Release);
                }
            });
            assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
            works.push(work);
        }

        for _ in 0..(MAX_WORKQUEUE_PENDING / 2) {
            assert!(run_one_work(queue));
        }

        for expected in MAX_WORKQUEUE_PENDING..(MAX_WORKQUEUE_PENDING + MAX_WORKQUEUE_PENDING / 2) {
            let work = WorkItem::new(move |_work| {
                let observed = WORK_OBSERVED.fetch_add(1, Ordering::Relaxed);
                if observed != expected {
                    STORE_BEFORE_QUEUE.store(expected + 1, Ordering::Release);
                }
            });
            assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
            works.push(work);
        }

        while run_one_work(queue) {}

        assert_eq!(
            WORK_OBSERVED.load(Ordering::Relaxed),
            MAX_WORKQUEUE_PENDING + MAX_WORKQUEUE_PENDING / 2
        );
        assert_eq!(STORE_BEFORE_QUEUE.load(Ordering::Acquire), 0);
    }

    #[def_test(serial)]
    fn test_cancel_work_removes_wrapped_pending_entry() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let mut works = Vec::new();

        WORK_RUNS.store(0, Ordering::Relaxed);
        for _ in 0..MAX_WORKQUEUE_PENDING {
            let work = test_work(count_work);
            assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
            works.push(work);
        }

        for _ in 0..(MAX_WORKQUEUE_PENDING / 2) {
            assert!(run_one_work(queue));
        }

        let wrapped = test_work(count_work);
        assert_eq!(queue_work(queue, &wrapped), QueueWorkResult::Queued);
        assert_eq!(cancel_work(&wrapped), CancelWorkResult::CancelledPending);

        while run_one_work(queue) {}
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), MAX_WORKQUEUE_PENDING);
    }

    #[def_test(serial)]
    fn test_queue_work_rejects_canceling_work() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = test_work(count_work);

        work.work().state.lock().status = WorkStatus::Canceling;
        assert_eq!(queue_work(queue, &work), QueueWorkResult::Disabled);
        assert_eq!(pending_len_for_tests(queue), 0);
    }

    #[def_test(serial)]
    fn test_non_waiting_cancel_removes_pending_work() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = test_work(count_work);

        WORK_RUNS.store(0, Ordering::Relaxed);
        assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        assert_eq!(cancel_work(&work), CancelWorkResult::CancelledPending);
        assert!(!run_one_work(queue));
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);
    }

    #[def_test(serial)]
    fn test_flush_work_reports_idle_work() {
        let work = test_work(count_work);

        assert_eq!(flush_work(&work), Ok(false));
    }

    #[def_test(serial)]
    fn test_flush_work_rejects_self_wait_from_explicit_worker() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = test_work(self_flush_work);

        SELF_WAIT_RESULT.store(0, Ordering::Release);
        assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        assert!(run_one_work(queue));
        assert_eq!(SELF_WAIT_RESULT.load(Ordering::Acquire), 2);
    }

    #[def_test(serial)]
    fn test_run_one_work_rejects_nested_drain() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let _queue = ScopedTestQueue::install(&NESTED_TEST_QUEUE, queue);
        let outer = test_work(nested_run_work);
        let inner = test_work(count_work);

        WORK_RUNS.store(0, Ordering::Relaxed);
        NESTED_RUN_RESULT.store(usize::MAX, Ordering::Release);
        assert_eq!(queue_work(queue, &outer), QueueWorkResult::Queued);
        assert_eq!(queue_work(queue, &inner), QueueWorkResult::Queued);

        assert!(run_one_work(queue));
        assert_eq!(NESTED_RUN_RESULT.load(Ordering::Acquire), 0);
        assert_eq!(pending_len_for_tests(queue), 1);
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);

        assert!(run_one_work(queue));
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
        assert_eq!(pending_len_for_tests(queue), 0);
    }

    #[def_test(serial)]
    fn test_flush_work_rejects_same_queue_pending_wait_from_callback() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let outer = test_work(flush_other_pending_work);
        let inner = test_work(count_work);

        WORK_RUNS.store(0, Ordering::Relaxed);
        SAME_QUEUE_WAIT_RESULT.store(0, Ordering::Release);
        *SAME_QUEUE_WAIT_TARGET.lock() = Some(inner.clone());

        assert_eq!(queue_work(queue, &outer), QueueWorkResult::Queued);
        assert_eq!(queue_work(queue, &inner), QueueWorkResult::Queued);
        assert!(run_one_work(queue));
        assert_eq!(SAME_QUEUE_WAIT_RESULT.load(Ordering::Acquire), 2);
        assert_eq!(pending_len_for_tests(queue), 1);

        assert!(run_one_work(queue));
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 1);
        *SAME_QUEUE_WAIT_TARGET.lock() = None;
    }

    #[def_test(serial)]
    fn test_flush_work_rejects_running_work_from_worker_callback() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let outer = test_work(count_work);
        let target = test_work(count_work);

        target.work().state.lock().status = WorkStatus::Running;
        let _current = CurrentWorkGuard::enter(queue, &outer);

        assert_eq!(flush_work(&target), Err(WorkqueueError::SelfWait));
    }

    #[def_test(serial)]
    fn test_cancel_work_sync_removes_pending_work() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = test_work(count_work);

        WORK_RUNS.store(0, Ordering::Relaxed);
        assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        assert_eq!(cancel_work_sync(&work), Ok(true));
        assert!(!run_one_work(queue));
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 0);
    }

    #[def_test(serial)]
    fn test_running_work_can_request_one_followup_run() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let _queue = ScopedTestQueue::install(&REQUEUE_TEST_QUEUE, queue);
        let work = test_work(requeue_once_work);

        WORK_RUNS.store(0, Ordering::Relaxed);
        assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        assert!(run_one_work(queue));
        assert_eq!(pending_len_for_tests(queue), 1);
        assert!(work.work().state.lock().pending_queue_is(queue));
        assert!(run_one_work(queue));
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 2);
    }

    #[def_test(serial)]
    fn test_cross_queue_requeue_keeps_followup_owner_queue() {
        let running_queue = Box::leak(Box::new(WorkQueue::new("running")));
        let followup_queue = Box::leak(Box::new(WorkQueue::new("followup")));
        let _queue = ScopedTestQueue::install(&CROSS_REQUEUE_TEST_QUEUE, followup_queue);
        let work = test_work(cross_requeue_once_work);

        WORK_RUNS.store(0, Ordering::Relaxed);
        assert_eq!(queue_work(running_queue, &work), QueueWorkResult::Queued);
        assert!(run_one_work(running_queue));
        assert_eq!(pending_len_for_tests(running_queue), 0);
        assert_eq!(pending_len_for_tests(followup_queue), 1);
        assert!(work.work().state.lock().pending_queue_is(followup_queue));

        assert!(!run_one_work(running_queue));
        assert!(run_one_work(followup_queue));
        assert_eq!(WORK_RUNS.load(Ordering::Relaxed), 2);
    }

    #[def_test(serial)]
    fn test_system_wq_runnable_query_ignores_running_followup() {
        let running_queue = Box::leak(Box::new(WorkQueue::new("running")));
        let _queue = ScopedTestQueue::install(&CROSS_REQUEUE_TEST_QUEUE, system_wq());
        let work = test_work(cross_requeue_once_work);

        WORK_RUNS.store(0, Ordering::Relaxed);
        assert_eq!(queue_work(running_queue, &work), QueueWorkResult::Queued);
        assert!(take_runnable_work(running_queue).is_some());
        work.run();
        assert!(!system_wq_has_runnable_work());

        assert_eq!(cancel_work(&work), CancelWorkResult::CancelledPending);
        finish_work(running_queue, &work);
    }

    #[def_test(serial)]
    fn test_queue_work_is_allowed_in_hardirq_context() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = test_work(count_work);

        {
            let _hardirq = HardIrqContextGuard::enter();
            assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        }
        assert!(run_one_work(queue));
    }

    #[def_test(serial)]
    fn test_waiting_work_apis_reject_hardirq_context() {
        let work = test_work(count_work);

        let _hardirq = HardIrqContextGuard::enter();
        assert_eq!(flush_work(&work), Err(WorkqueueError::InvalidContext));
        assert_eq!(cancel_work_sync(&work), Err(WorkqueueError::InvalidContext));
    }

    #[def_test(serial)]
    fn test_queue_work_is_allowed_with_bh_disabled() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let work = test_work(count_work);

        {
            let _bh = local_bh_disable();
            assert_eq!(queue_work(queue, &work), QueueWorkResult::Queued);
        }
        assert!(run_one_work(queue));
    }

    #[def_test(serial)]
    fn test_queue_work_is_allowed_from_softirq_action() {
        let queue = Box::leak(Box::new(WorkQueue::new("test")));
        let _queue = ScopedTestQueue::install(&SOFTIRQ_TEST_QUEUE, queue);
        let _action = ScopedSoftirqAction::install(SoftirqVec::Block, queue_from_softirq_action);

        raise_softirq(SoftirqVec::Block);
        let _ = crate::softirq::run_pending_softirqs();

        assert_eq!(pending_len_for_tests(queue), 1);
        assert!(run_one_work(queue));
    }
}
