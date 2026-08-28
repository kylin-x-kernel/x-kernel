// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Built-in workqueue runtime helpers.

use alloc::{sync::Arc, task::Wake};
use core::{
    future::poll_fn,
    sync::atomic::{AtomicBool, Ordering},
    task::{Poll, Waker},
};

use kcpu_id_map::LogicalCpuId;
use kirq::softirq::{SoftirqVec, open_softirq, raise_softirq};
use kpoll::{PollEvent, PollRegistrations};
use kspin::SpinNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};
use kworkerpool::{
    ActionBatch, PoolEntry, PoolId, RunnableClaim, RunnableClaimer, WorkerExecutionToken, WorkerId,
    WorkerRuntime,
};
use log::{debug, warn};

use crate::{
    ScheduledWork,
    builtinpool::{BuiltinPoolRuntime, SystemPoolBinding, SystemPoolKind},
    builtinwq::BottomHalfWorkQueueKind,
    work::{
        WorkQueueRef, binding_for_executor_entry, release_scheduled_work, scheduled_work_by_key,
    },
};

const BH_WORKER_TIMESLICE: TimeSpan = TimeSpan::from_millis(2);
const BH_WORKER_RESTARTS: usize = 10;
/// Initializes built-in workqueue worker pools for one CPU.
pub fn init_system_workqueue_worker_pools_for_cpu(
    cpu_id: LogicalCpuId,
) -> Option<crate::builtinpool::BuiltinCpuPoolInitResult> {
    crate::builtinpool::init_system_worker_pools_for_cpu(cpu_id, BuiltinWorkerRuntime::new)
}

/// Timer handle used by delayed work.
pub(crate) trait WorkqueueTimerHandle: Send + Sync {
    /// Cancels the timer.
    fn cancel(&self) -> bool;
}

struct TimerWake {
    done: AtomicBool,
    callback: Arc<dyn Fn() + Send + Sync>,
}

impl TimerWake {
    fn new(callback: Arc<dyn Fn() + Send + Sync>) -> Self {
        Self {
            done: AtomicBool::new(false),
            callback,
        }
    }

    fn cancel(&self) -> bool {
        !self.done.swap(true, Ordering::AcqRel)
    }

    fn fire(&self) {
        if !self.done.swap(true, Ordering::AcqRel) {
            (self.callback)();
        }
    }
}

impl Wake for TimerWake {
    fn wake(self: Arc<Self>) {
        self.fire();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        self.fire();
    }
}

/// Returns the logical CPU currently executing the caller.
pub(crate) fn current_cpu_id() -> LogicalCpuId {
    ktask::current_may_uninit()
        .map(|task| task.cpu_id())
        .unwrap_or(LogicalCpuId::new(0))
}

/// Arms a one-shot timer.
pub(crate) fn arm_timer(
    deadline: MonotonicInstant,
    callback: Arc<dyn Fn() + Send + Sync>,
) -> Option<Arc<dyn WorkqueueTimerHandle>> {
    struct TimerHandle {
        wake: Arc<TimerWake>,
        handle: SpinNoIrq<Option<ktask::future::TimerHandle>>,
    }

    impl WorkqueueTimerHandle for TimerHandle {
        fn cancel(&self) -> bool {
            if let Some(handle) = self.handle.lock().take() {
                ktask::future::cancel_timer(&handle);
            }
            self.wake.cancel()
        }
    }

    let wake = Arc::new(TimerWake::new(callback));
    let waker = Waker::from(wake.clone());
    let handle = ktask::future::register_timer(deadline, waker)?;
    Some(Arc::new(TimerHandle {
        wake,
        handle: SpinNoIrq::new(Some(handle)),
    }))
}

#[kiface::provide]
impl ktask::TaskExecutionAccountingIf {
    fn account_execution_tick(context: ktask::TaskExecutionContext) -> Option<MonotonicInstant> {
        crate::builtinpool::account_system_execution_tick(context, ktask::monotonic_time())
    }

    fn execution_tick_deadline(context: ktask::TaskExecutionContext) -> Option<MonotonicInstant> {
        crate::builtinpool::system_execution_tick_deadline(context)
    }
}

/// Runtime hook used by ktask-backed built-in worker-pool workers.
pub(crate) struct BuiltinWorkerRuntime {
    pool_kind: SystemPoolKind,
    cpu_id: LogicalCpuId,
    wake_source: Arc<PollEvent>,
}

impl BuiltinWorkerRuntime {
    pub(crate) fn new(pool_id: PoolId, _worker: WorkerId, wake_source: Arc<PollEvent>) -> Self {
        let pool_kind = SystemPoolKind::from_usize(pool_id.kind().as_usize())
            .expect("built-in worker runtime must use a built-in pool kind");
        Self {
            pool_kind,
            cpu_id: pool_id.cpu(),
            wake_source,
        }
    }
}

pub(crate) struct ClaimedBuiltinWork {
    work: ScheduledWork,
    queue: WorkQueueRef,
    cpu_id: LogicalCpuId,
    owner: kworkqueue::EntryOwner,
    bottom_half_kind: Option<BottomHalfWorkQueueKind>,
    claimed: kworkqueue::ClaimedWork,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum BuiltinClaimStale {
    InvalidEntry,
    UnknownOwner,
    UnknownWork,
    QueueRejected,
}

impl RunnableClaimer for BuiltinWorkerRuntime {
    type Accepted = ClaimedBuiltinWork;
    type Stale = BuiltinClaimStale;

    fn claim(
        &mut self,
        worker: WorkerId,
        token: WorkerExecutionToken,
        entry: PoolEntry,
    ) -> RunnableClaim<Self::Accepted, Self::Stale> {
        claim_builtin_pool_entry(self.pool_kind, self.cpu_id, worker, token, entry)
    }

    fn record_stale(&mut self, stale: Self::Stale) {
        debug!("discarded stale built-in workqueue entry: {:?}", stale);
    }
}

impl WorkerRuntime<&'static BuiltinPoolRuntime> for BuiltinWorkerRuntime {
    fn now(&self) -> MonotonicInstant {
        ktask::monotonic_time()
    }

    fn wait_for_worker_work(&mut self, _pool: &'static BuiltinPoolRuntime, _worker: WorkerId) {
        wait_for_worker_work(_pool, _worker, &self.wake_source);
    }

    fn handle_worker_actions(&mut self, _pool: &'static BuiltinPoolRuntime, actions: ActionBatch) {
        crate::builtinpool::handle_actions(actions);
    }

    fn run_claimed_work(
        &mut self,
        _pool: &'static BuiltinPoolRuntime,
        _worker: WorkerId,
        _token: WorkerExecutionToken,
        work: Self::Accepted,
    ) {
        run_claimed_builtin_work(self.pool_kind, self.cpu_id, work);
    }
}

fn claim_builtin_pool_entry(
    pool_kind: SystemPoolKind,
    cpu_id: LogicalCpuId,
    worker: WorkerId,
    token: WorkerExecutionToken,
    entry: PoolEntry,
) -> RunnableClaim<ClaimedBuiltinWork, BuiltinClaimStale> {
    let Some(entry) = crate::builtinpool::executor_entry(entry) else {
        return RunnableClaim::Stale(BuiltinClaimStale::InvalidEntry);
    };
    let Some(executor_binding) = binding_for_executor_entry(pool_kind, cpu_id, entry) else {
        return RunnableClaim::Stale(BuiltinClaimStale::UnknownOwner);
    };
    let queue = executor_binding.queue;
    let Some(binding) = queue.queue().core_binding(executor_binding.cpu_id) else {
        return RunnableClaim::Stale(BuiltinClaimStale::UnknownOwner);
    };
    let Some(work_key) = binding.work_key_for_entry(entry) else {
        return RunnableClaim::Stale(BuiltinClaimStale::UnknownWork);
    };
    let Some(work) = scheduled_work_by_key(work_key) else {
        return RunnableClaim::Stale(BuiltinClaimStale::UnknownWork);
    };

    match binding.claim(entry, work.core(), worker.as_usize(), token.as_usize()) {
        kworkqueue::ClaimResult::Run(claimed) => RunnableClaim::Run(ClaimedBuiltinWork {
            work,
            queue,
            cpu_id: executor_binding.cpu_id,
            owner: entry.owner,
            bottom_half_kind: executor_binding.bottom_half_kind,
            claimed,
        }),
        kworkqueue::ClaimResult::Stale => RunnableClaim::Stale(BuiltinClaimStale::QueueRejected),
    }
}

fn run_claimed_builtin_work(
    pool_kind: SystemPoolKind,
    cpu_id: LogicalCpuId,
    claimed: ClaimedBuiltinWork,
) {
    claimed.work.run();
    let Some(binding) = claimed.queue.queue().core_binding(claimed.cpu_id) else {
        warn!("built-in workqueue finish lost its queue binding");
        return;
    };
    match binding.finish(claimed.work.core(), claimed.claimed) {
        kworkqueue::FinishResult::Finished {
            requeue_op,
            promote_op,
            cancel_complete: _,
        } => {
            if requeue_op.is_none() {
                claimed.work.clear_binding_if_owner(claimed.owner);
                release_scheduled_work(claimed.work.core().key());
            }
            claimed.work.notify_state_change();
            claimed.queue.queue().notify_state_change();
            if let Some(op) = requeue_op
                && let Some(pool) = crate::builtinpool::system_pool_for_kind_cpu(pool_kind, cpu_id)
            {
                let _ = pool.apply_executor_op(op);
                if let Some(kind) = claimed.queue.bottom_half_kind() {
                    raise_bottom_half_workqueue_on(cpu_id, kind);
                }
            }
            if let Some(op) = promote_op
                && let Some(pool) = crate::builtinpool::system_pool_for_kind_cpu(pool_kind, cpu_id)
            {
                match op {
                    kworkqueue::ExecutorOp::PromoteInactive { owner, budget } => {
                        let _ = crate::work::apply_promote_inactive_for_queue(
                            pool_kind,
                            claimed.bottom_half_kind,
                            cpu_id,
                            binding,
                            owner,
                            budget,
                        );
                    }
                    _ => {
                        let _ = pool.apply_executor_op(op);
                    }
                }
            }
        }
        kworkqueue::FinishResult::Stale => {
            warn!("built-in workqueue finish rejected a claimed work item");
        }
    }
}

fn wait_for_worker_work(
    pool: &'static BuiltinPoolRuntime,
    worker: WorkerId,
    wake_source: &PollEvent,
) {
    let mut registrations = PollRegistrations::new();
    loop {
        let observed_generation = wake_source.generation();
        let event = poll_fn(|cx| {
            if pool.lock().worker_wait_ready(worker) {
                return Poll::Ready(true);
            }
            let mut context = registrations.context(cx);
            if wake_source.register(&mut context).is_err() {
                drop(context);
                return Poll::Ready(false);
            }
            drop(context);
            if pool.lock().worker_wait_ready(worker)
                || wake_source.has_changed_since(observed_generation)
            {
                Poll::Ready(true)
            } else {
                Poll::Pending
            }
        });
        match ktask::future::block_on(event) {
            true => return,
            false => ktask::yield_now(),
        }
    }
}

/// Installs bottom-half workerqueue softirq actions.
///
/// `kwork` owns the BH workqueue and pool state. `kirq` only supplies the
/// softirq execution slot used to drain that state.
pub fn init_bottom_half_workerqueue() -> bool {
    let default_installed = open_softirq(SoftirqVec::Tasklet, drain_default_bh_workqueue);
    let highpri_installed = open_softirq(SoftirqVec::High, drain_highpri_bh_workqueue);
    default_installed && highpri_installed
}

/// Opaque binding for a built-in bottom-half queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct BottomHalfPoolBinding {
    kind: BottomHalfWorkQueueKind,
    cpu_id: LogicalCpuId,
}

impl BottomHalfPoolBinding {
    pub(crate) fn for_kind_cpu(
        kind: BottomHalfWorkQueueKind,
        cpu_id: LogicalCpuId,
    ) -> Option<Self> {
        (cpu_id.as_usize() < kbuild_config::NR_CPUS).then_some(Self { kind, cpu_id })
    }

    pub(crate) fn has_runnable_work(self) -> bool {
        crate::builtinpool::system_pool_for_kind_cpu(SystemPoolKind::Bh, self.cpu_id)
            .is_some_and(|pool| pool.pool().lock().runnable_len() != 0)
    }
}

pub(crate) fn process_one_bottom_half_pool_work(binding: BottomHalfPoolBinding) -> bool {
    let Some(pool) =
        crate::builtinpool::system_pool_for_kind_cpu(SystemPoolKind::Bh, binding.cpu_id)
    else {
        return false;
    };
    run_one_bottom_half_pool_work(binding.kind, pool)
}

/// Returns whether the current context may not block.
pub(crate) fn is_invalid_wait_context() -> bool {
    kirq::context::is_in_interrupt_context()
}

pub(crate) fn raise_bottom_half_workqueue_on(cpu_id: LogicalCpuId, kind: BottomHalfWorkQueueKind) {
    let vec = softirq_vec_for_kind(kind);
    if cpu_id == current_cpu_id() {
        raise_softirq(vec);
        return;
    }

    if let Err(error) = kipi::run_on_cpu(cpu_id, move || raise_softirq(vec)) {
        warn!(
            "failed to raise bottom-half workqueue softirq on CPU {}: {error:?}",
            cpu_id.as_usize()
        );
    }
}

fn drain_default_bh_workqueue() {
    drain_bh_workqueue(BottomHalfWorkQueueKind::Default);
}

fn drain_highpri_bh_workqueue() {
    drain_bh_workqueue(BottomHalfWorkQueueKind::HighPri);
}

fn drain_bh_workqueue(kind: BottomHalfWorkQueueKind) {
    let cpu_id = current_cpu_id();
    let Some(binding) = BottomHalfPoolBinding::for_kind_cpu(kind, cpu_id) else {
        return;
    };
    let mut budget = BottomHalfDrainBudget::new();

    loop {
        if !process_one_bottom_half_pool_work(binding) {
            return;
        }
        budget.record_completed_work();
        if !budget.can_continue() {
            break;
        }
    }

    if binding.has_runnable_work() {
        raise_softirq(softirq_vec_for_kind(kind));
    }
}

fn run_one_bottom_half_pool_work(kind: BottomHalfWorkQueueKind, pool: SystemPoolBinding) -> bool {
    let worker = WorkerId::new(0);
    let now = ktask::monotonic_time();
    let Ok((decision, actions)) = pool.pool().lock().worker_ready_to_park(worker, now) else {
        return false;
    };
    crate::builtinpool::handle_actions(actions);
    if decision != kworkerpool::ParkDecision::Run {
        return false;
    }

    let candidate = pool
        .pool()
        .lock()
        .prepare_runnable_candidate(worker, ktask::monotonic_time());
    let Ok(kworkerpool::RunnableCandidateResult::Candidate(candidate)) = candidate else {
        return false;
    };
    crate::builtinpool::handle_actions(candidate.actions);

    match claim_builtin_pool_entry(
        SystemPoolKind::Bh,
        pool.cpu_id(),
        worker,
        candidate.token,
        candidate.entry,
    ) {
        RunnableClaim::Run(claimed) => {
            let Ok(actions) = pool.pool().lock().commit_runnable_candidate(
                worker,
                candidate.token,
                ktask::monotonic_time(),
            ) else {
                warn!("bottom-half workqueue claim commit failed");
                return false;
            };
            crate::builtinpool::handle_actions(actions);
            let _ = kind;
            run_claimed_builtin_work(SystemPoolKind::Bh, pool.cpu_id(), claimed);
            if let Ok(actions) =
                pool.pool()
                    .lock()
                    .worker_finished(worker, candidate.token, ktask::monotonic_time())
            {
                crate::builtinpool::handle_actions(actions);
            }
            true
        }
        RunnableClaim::Stale(stale) => {
            debug!("discarded stale bottom-half workqueue entry: {:?}", stale);
            if let Ok(discarded) = pool.pool().lock().discard_runnable_candidate(
                worker,
                candidate.token,
                ktask::monotonic_time(),
            ) {
                crate::builtinpool::handle_actions(discarded.actions);
            }
            false
        }
    }
}

struct BottomHalfDrainBudget {
    restarts_left: usize,
    deadline: MonotonicInstant,
}

impl BottomHalfDrainBudget {
    fn new() -> Self {
        let now = ktask::monotonic_time();
        Self {
            restarts_left: BH_WORKER_RESTARTS,
            deadline: now
                .checked_add(BH_WORKER_TIMESLICE)
                .unwrap_or(MonotonicInstant::from_span_since_origin(TimeSpan::MAX)),
        }
    }

    fn record_completed_work(&mut self) {
        self.restarts_left = self.restarts_left.saturating_sub(1);
    }

    fn can_continue(&self) -> bool {
        self.restarts_left > 0 && ktask::monotonic_time() < self.deadline
    }
}

const fn softirq_vec_for_kind(kind: BottomHalfWorkQueueKind) -> SoftirqVec {
    match kind {
        BottomHalfWorkQueueKind::Default => SoftirqVec::Tasklet,
        BottomHalfWorkQueueKind::HighPri => SoftirqVec::High,
    }
}
