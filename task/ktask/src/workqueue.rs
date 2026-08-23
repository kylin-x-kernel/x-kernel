// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `ktask` provider for the generic kwork system workerqueue.

use alloc::{format, string::String, sync::Arc};
#[cfg(unittest)]
use core::sync::atomic::{AtomicUsize, Ordering};
use core::{
    future::poll_fn,
    task::{Poll, Waker},
};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use kpoll::{Completion, PollEvent, PollRegistrations, PollSet};
use kspin::{NoPreempt, SpinNoIrq};
use ktime_types::MonotonicInstant;

use crate::{
    KCpuMask, KtaskRef, TaskInner, activate_task, future::block_on, prepare_task, sleep, yield_now,
};

static SYSTEM_POOL_WAKE_SOURCES: SystemPoolWakeSources = SystemPoolWakeSources::new();
#[cfg(unittest)]
static WORKQUEUE_WAIT_PROBE: AtomicUsize = AtomicUsize::new(0);

#[cfg(unittest)]
fn arm_workqueue_wait_probe_for_tests() {
    WORKQUEUE_WAIT_PROBE.store(1, Ordering::Release);
}

#[cfg(unittest)]
fn workqueue_wait_probe_observed_for_tests() -> bool {
    WORKQUEUE_WAIT_PROBE.load(Ordering::Acquire) == 2
}

#[cfg(unittest)]
fn clear_workqueue_wait_probe_for_tests() {
    WORKQUEUE_WAIT_PROBE.store(0, Ordering::Release);
}

#[cfg(unittest)]
fn record_workqueue_wait_probe_for_tests() {
    if WORKQUEUE_WAIT_PROBE.load(Ordering::Acquire) == 1 {
        WORKQUEUE_WAIT_PROBE.store(2, Ordering::Release);
    }
}

struct SystemPoolWakeSourcesEntry {
    wake_sources: [PollSet; kwork::raw::MAX_SYSTEM_WORKERS_PER_CPU],
    worker_tasks: [Option<KtaskRef>; kwork::raw::MAX_SYSTEM_WORKERS_PER_CPU],
    manager_wake_source: PollSet,
    manager_task: Option<KtaskRef>,
}

struct SystemPoolWakeSources(
    [[SpinNoIrq<Option<SystemPoolWakeSourcesEntry>>; kbuild_config::NR_CPUS];
        kwork::raw::SystemWorkQueueKind::COUNT],
);

impl SystemPoolWakeSources {
    const fn new() -> Self {
        Self(
            [const { [const { SpinNoIrq::new(None) }; kbuild_config::NR_CPUS] };
                kwork::raw::SystemWorkQueueKind::COUNT],
        )
    }

    fn install(
        &self,
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
        wake_sources: [PollSet; kwork::raw::MAX_SYSTEM_WORKERS_PER_CPU],
        manager_wake_source: PollSet,
    ) -> bool {
        let Some(kind_slots) = self.0.get(kind.as_usize()) else {
            return false;
        };
        let Some(cpu_slot) = kind_slots.get(cpu_id.as_usize()) else {
            warn!(
                "cannot install {:?} system workerqueue wake source for out-of-range CPU {}",
                kind,
                cpu_id.as_usize()
            );
            return false;
        };

        let mut slot = cpu_slot.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(SystemPoolWakeSourcesEntry {
            wake_sources,
            worker_tasks: core::array::from_fn(|_| None),
            manager_wake_source,
            manager_task: None,
        });
        true
    }

    fn get(
        &self,
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
        worker_id: kwork::raw::WorkerId,
    ) -> Option<PollSet> {
        self.0
            .get(kind.as_usize())?
            .get(cpu_id.as_usize())
            .and_then(|slot| {
                slot.lock()
                    .as_ref()
                    .and_then(|pool| pool.wake_sources.get(worker_id.as_usize()).cloned())
            })
    }

    fn is_installed(&self, kind: kwork::raw::SystemWorkQueueKind, cpu_id: LogicalCpuId) -> bool {
        self.0
            .get(kind.as_usize())
            .and_then(|slots| slots.get(cpu_id.as_usize()))
            .is_some_and(|slot| slot.lock().is_some())
    }

    fn manager(
        &self,
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
    ) -> Option<PollSet> {
        self.0
            .get(kind.as_usize())?
            .get(cpu_id.as_usize())
            .and_then(|slot| {
                slot.lock()
                    .as_ref()
                    .map(|pool| pool.manager_wake_source.clone())
            })
    }

    fn register_worker_task(
        &self,
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
        worker_id: kwork::raw::WorkerId,
        task: KtaskRef,
    ) -> bool {
        self.0
            .get(kind.as_usize())
            .and_then(|slots| slots.get(cpu_id.as_usize()))
            .is_some_and(|slot| {
                let mut slot = slot.lock();
                let Some(entry) = slot.as_mut() else {
                    return false;
                };
                let Some(task_slot) = entry.worker_tasks.get_mut(worker_id.as_usize()) else {
                    return false;
                };
                if task_slot.is_some() {
                    return false;
                }
                *task_slot = Some(task);
                true
            })
    }

    fn worker_task(
        &self,
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
        worker_id: kwork::raw::WorkerId,
    ) -> Option<KtaskRef> {
        self.0
            .get(kind.as_usize())?
            .get(cpu_id.as_usize())
            .and_then(|slot| {
                slot.lock()
                    .as_ref()
                    .and_then(|entry| entry.worker_tasks.get(worker_id.as_usize()).cloned())
                    .flatten()
            })
    }

    fn unregister_worker_task(
        &self,
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
        worker_id: kwork::raw::WorkerId,
    ) -> Option<KtaskRef> {
        self.0
            .get(kind.as_usize())?
            .get(cpu_id.as_usize())
            .and_then(|slot| {
                slot.lock()
                    .as_mut()
                    .and_then(|entry| entry.worker_tasks.get_mut(worker_id.as_usize()))
                    .and_then(Option::take)
            })
    }

    fn register_manager_task(
        &self,
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
        task: KtaskRef,
    ) -> bool {
        self.0
            .get(kind.as_usize())
            .and_then(|slots| slots.get(cpu_id.as_usize()))
            .is_some_and(|slot| {
                let mut slot = slot.lock();
                let Some(entry) = slot.as_mut() else {
                    return false;
                };
                if entry.manager_task.is_some() {
                    return false;
                }
                entry.manager_task = Some(task);
                true
            })
    }

    fn manager_task(
        &self,
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
    ) -> Option<KtaskRef> {
        self.0
            .get(kind.as_usize())?
            .get(cpu_id.as_usize())
            .and_then(|slot| {
                slot.lock()
                    .as_ref()
                    .and_then(|entry| entry.manager_task.clone())
            })
    }
}

/// Starts the current CPU's bounded system workerqueue pool if it has not been
/// started yet.
///
/// The system queue uses a bounded pool of `kwork/system_wq/N:M` tasks per
/// CPU. Each task drains the current CPU's kwork-owned system pool; a sleeping
/// callback only blocks that worker task, while other pre-created workers can
/// continue draining runnable work.
pub fn init_system_workqueue_worker() {
    init_system_workqueue_kind_worker(kwork::raw::SystemWorkQueueKind::Default);
}

fn init_system_workqueue_kind_worker(kind: kwork::raw::SystemWorkQueueKind) {
    let wake_sources = core::array::from_fn(|_| PollSet::new());
    let manager_wake_source = PollSet::new();
    let cpu_id = khal::percpu::this_cpu_id();
    if !SYSTEM_POOL_WAKE_SOURCES.install(
        kind,
        cpu_id,
        wake_sources.clone(),
        manager_wake_source.clone(),
    ) {
        return;
    }

    for worker_id in 0..kwork::raw::INITIAL_SYSTEM_WORKERS_PER_CPU {
        let worker_id = kwork::raw::WorkerId::new(worker_id);
        let worker_wake_source = wake_sources[worker_id.as_usize()].clone();
        if !start_system_worker_task(kind, cpu_id, worker_id, worker_wake_source) {
            warn!(
                "failed to start initial {:?} system workerqueue worker {} for CPU {}",
                kind,
                worker_id.as_usize(),
                cpu_id.as_usize()
            );
        }
    }

    let Some(pool) = kwork::raw::SystemPoolBinding::for_kind_cpu(kind, cpu_id) else {
        warn!(
            "cannot start {:?} system workerqueue manager for out-of-range CPU {}",
            kind,
            cpu_id.as_usize()
        );
        return;
    };
    let manager_task = TaskInner::new_pidless_kthread(
        move || system_worker_manager_main(kind, cpu_id, manager_wake_source),
        system_worker_manager_name(kind, cpu_id),
        kbuild_config::TASK_STACK_SIZE,
    );
    let manager_task = prepare_task(manager_task);
    manager_task.set_cpumask(system_pool_cpumask(pool));
    if !SYSTEM_POOL_WAKE_SOURCES.register_manager_task(kind, cpu_id, manager_task.clone()) {
        warn!(
            "failed to register {:?} system workerqueue manager task for CPU {}",
            kind,
            cpu_id.as_usize()
        );
    }
    activate_task(&manager_task);
}

#[kiface::provide]
impl kwork::raw::WorkqueueHostIf {
    fn current_cpu_id() -> LogicalCpuId {
        khal::percpu::this_cpu_id()
    }

    fn is_system_pool_ready(kind: kwork::raw::SystemWorkQueueKind, cpu_id: LogicalCpuId) -> bool {
        SYSTEM_POOL_WAKE_SOURCES.is_installed(kind, cpu_id)
    }

    fn wake_system_worker(
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
        worker_id: kwork::raw::WorkerId,
    ) {
        let _guard = NoPreempt::new();
        if let Some(source) = SYSTEM_POOL_WAKE_SOURCES.get(kind, cpu_id, worker_id) {
            let _ = source.wake();
        }
    }

    fn wake_system_manager(kind: kwork::raw::SystemWorkQueueKind, cpu_id: LogicalCpuId) {
        let _guard = NoPreempt::new();
        if let Some(source) = SYSTEM_POOL_WAKE_SOURCES.manager(kind, cpu_id) {
            let _ = source.wake();
        }
    }
}

#[kiface::provide]
impl kwork::raw::WorkqueueTaskContextIf {
    fn set_current_work_context(
        context: kwork::raw::WorkqueueTaskContext,
    ) -> Option<kwork::raw::WorkqueueTaskContext> {
        crate::current_may_uninit().as_ref().and_then(|current| {
            current
                .set_workerqueue_current_work_context(
                    context.work_key(),
                    context.queue_key(),
                    context.pool_key(),
                    context.worker_id().as_usize(),
                    context.worker_token().as_usize(),
                )
                .map(|(work_key, queue_key, pool_key, worker_id, worker_token)| {
                    kwork::raw::WorkqueueTaskContext::new(
                        work_key,
                        queue_key,
                        pool_key,
                        kwork::raw::WorkerId::new(worker_id),
                        kwork::raw::WorkerExecutionToken::from_usize(worker_token),
                    )
                })
        })
    }

    fn clear_current_work_context(context: kwork::raw::WorkqueueTaskContext) -> bool {
        crate::current_may_uninit().as_ref().is_some_and(|current| {
            current.clear_workerqueue_current_work_context(
                context.work_key(),
                context.queue_key(),
                context.pool_key(),
                context.worker_id().as_usize(),
                context.worker_token().as_usize(),
            )
        })
    }

    fn current_work_context() -> Option<kwork::raw::WorkqueueTaskContext> {
        crate::current_may_uninit().as_ref().and_then(|current| {
            current.workerqueue_current_work_context().map(
                |(work_key, queue_key, pool_key, worker_id, worker_token)| {
                    kwork::raw::WorkqueueTaskContext::new(
                        work_key,
                        queue_key,
                        pool_key,
                        kwork::raw::WorkerId::new(worker_id),
                        kwork::raw::WorkerExecutionToken::from_usize(worker_token),
                    )
                },
            )
        })
    }
}

#[kiface::provide]
impl kwork::raw::WorkqueueSyncWaitIf {
    fn wait_for_completion(completion: &kpoll::Completion) -> Result<(), kpoll::PollRegisterError> {
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
            completion.register(&mut context)?;
            drop(context);
            #[cfg(unittest)]
            record_workqueue_wait_probe_for_tests();

            if completion.try_wait() {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        }))
    }

    fn wait_for_completion_or_event(
        completion: &Completion,
        state_change: &PollEvent,
        observed_generation: usize,
    ) -> Result<(), kpoll::PollRegisterError> {
        if completion.try_wait() || state_change.has_changed_since(observed_generation) {
            return Ok(());
        }
        if crate::current_may_uninit().is_none() {
            return Err(kpoll::PollRegisterError::InvalidState);
        }

        let mut registrations = PollRegistrations::new();
        block_on(poll_fn(|cx| {
            if completion.try_wait() || state_change.has_changed_since(observed_generation) {
                return Poll::Ready(Ok(()));
            }

            let mut context = registrations.context(cx);
            completion.register(&mut context)?;
            state_change.register(&mut context)?;
            drop(context);
            #[cfg(unittest)]
            record_workqueue_wait_probe_for_tests();

            if completion.try_wait() || state_change.has_changed_since(observed_generation) {
                return Poll::Ready(Ok(()));
            }
            Poll::Pending
        }))
    }
}

#[kiface::provide]
impl kwork::raw::WorkqueueTimerIf {
    fn monotonic_time() -> MonotonicInstant {
        khal::time::monotonic_time()
    }

    fn register_timer(
        deadline: MonotonicInstant,
        waker: Waker,
    ) -> Option<Arc<dyn kwork::raw::WorkqueueTimerHandle>> {
        crate::future::register_timer(deadline, waker)
            .map(|handle| Arc::new(KtaskWorkqueueTimerHandle(handle)) as Arc<_>)
    }
}

struct KtaskWorkqueueTimerHandle(crate::future::TimerHandle);

impl kwork::raw::WorkqueueTimerHandle for KtaskWorkqueueTimerHandle {
    fn cancel(&self) {
        crate::future::cancel_timer(&self.0);
    }
}

fn system_worker_main(
    kind: kwork::raw::SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
    worker_id: kwork::raw::WorkerId,
    wake_source: PollSet,
) {
    debug!("started {}", worker_name(kind, cpu_id, worker_id));
    let Some(pool) = kwork::raw::SystemPoolBinding::for_kind_cpu(kind, cpu_id) else {
        warn!(
            "system workerqueue worker {} found no {:?} pool for CPU {}",
            worker_id.as_usize(),
            kind,
            cpu_id.as_usize()
        );
        return;
    };
    loop {
        wait_for_system_work(pool, worker_id, &wake_source);
        drain_system_workqueue(pool, worker_id);
    }
}

fn system_worker_manager_main(
    kind: kwork::raw::SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
    wake_source: PollSet,
) {
    debug!("started {}", system_worker_manager_name(kind, cpu_id));
    let Some(pool) = kwork::raw::SystemPoolBinding::for_kind_cpu(kind, cpu_id) else {
        warn!(
            "system workerqueue manager found no {:?} pool for CPU {}",
            kind,
            cpu_id.as_usize()
        );
        return;
    };
    loop {
        wait_for_system_manager_work(pool, &wake_source);
        while let Some(worker_id) = pool.reserve_worker_creation() {
            let worker_wake_source = match SYSTEM_POOL_WAKE_SOURCES.get(kind, cpu_id, worker_id) {
                Some(source) => source,
                None => {
                    pool.finish_worker_creation(worker_id, false);
                    break;
                }
            };
            let started = start_system_worker_task(kind, cpu_id, worker_id, worker_wake_source);
            pool.finish_worker_creation(worker_id, started);
            if !started {
                sleep(kwork::raw::WORKER_CREATE_RETRY_DELAY);
                break;
            }
        }
    }
}

fn start_system_worker_task(
    kind: kwork::raw::SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
    worker_id: kwork::raw::WorkerId,
    wake_source: PollSet,
) -> bool {
    let Some(pool) = kwork::raw::SystemPoolBinding::for_kind_cpu(kind, cpu_id) else {
        return false;
    };
    let task = TaskInner::new_pidless_kthread(
        move || system_worker_main(kind, cpu_id, worker_id, wake_source),
        worker_name(kind, cpu_id, worker_id),
        kbuild_config::TASK_STACK_SIZE,
    );
    let task = prepare_task(task);
    task.set_cpumask(system_pool_cpumask(pool));
    if !SYSTEM_POOL_WAKE_SOURCES.register_worker_task(kind, cpu_id, worker_id, task.clone()) {
        warn!(
            "failed to register {:?} system workerqueue worker task {} for CPU {}",
            kind,
            worker_id.as_usize(),
            cpu_id.as_usize()
        );
        return false;
    }
    if !pool.install_worker(worker_id) {
        let _ = SYSTEM_POOL_WAKE_SOURCES.unregister_worker_task(kind, cpu_id, worker_id);
        warn!(
            "failed to install {:?} system workerqueue worker {} for CPU {}",
            kind,
            worker_id.as_usize(),
            cpu_id.as_usize()
        );
        return false;
    }
    activate_task(&task);
    true
}

fn system_pool_cpumask(pool: kwork::raw::SystemPoolBinding) -> KCpuMask {
    match pool.attrs().cpu_affinity() {
        kwork::raw::WorkerPoolCpuAffinity::Pinned(cpu_id) => KCpuMask::one_shot_logical(cpu_id),
    }
}

pub(crate) fn system_worker_task_for_wake(
    kind: kwork::raw::SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
    worker_id: kwork::raw::WorkerId,
) -> Option<KtaskRef> {
    SYSTEM_POOL_WAKE_SOURCES.worker_task(kind, cpu_id, worker_id)
}

pub(crate) fn system_manager_task_for_wake(
    kind: kwork::raw::SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
) -> Option<KtaskRef> {
    SYSTEM_POOL_WAKE_SOURCES.manager_task(kind, cpu_id)
}

fn drain_system_workqueue(pool: kwork::raw::SystemPoolBinding, worker_id: kwork::raw::WorkerId) {
    loop {
        // Test-only: a frozen pool is drained exclusively by the test task;
        // stop before each take so live workers cannot race a manual drain.
        #[cfg(unittest)]
        if pool.wakes_frozen_for_tests() {
            return;
        }
        if !pool.run_one_work(worker_id) {
            return;
        }
        yield_now();
    }
}

fn wait_for_system_work(
    pool: kwork::raw::SystemPoolBinding,
    worker_id: kwork::raw::WorkerId,
    wake_source: &PollSet,
) {
    let _ = wait_for_workqueue_event(wake_source, "system workerqueue waiter", || {
        if pool.prepare_worker_to_wait(worker_id) {
            WorkqueueWaitState::Ready
        } else {
            WorkqueueWaitState::Pending
        }
    });
}

fn wait_for_system_manager_work(pool: kwork::raw::SystemPoolBinding, wake_source: &PollSet) {
    let _ = wait_for_workqueue_event(wake_source, "system workerqueue manager", || {
        if pool.manager_needed() {
            WorkqueueWaitState::Ready
        } else {
            WorkqueueWaitState::Pending
        }
    });
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkqueueWaitState {
    Pending,
    Ready,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum WorkqueueWaitResult {
    Ready,
    RetryAfterYield,
}

fn wait_for_workqueue_event(
    wake_source: &PollSet,
    waiter_name: &'static str,
    mut check_state: impl FnMut() -> WorkqueueWaitState,
) -> WorkqueueWaitResult {
    let mut registrations = PollRegistrations::new();
    loop {
        match block_on(poll_fn(|cx| {
            match check_state() {
                WorkqueueWaitState::Ready => return Poll::Ready(WorkqueueWaitResult::Ready),
                WorkqueueWaitState::Pending => {}
            }

            let mut context = registrations.context(cx);
            if let Err(error) = context.register(wake_source) {
                warn!("failed to register {waiter_name}: {error:?}");
                drop(context);
                return Poll::Ready(WorkqueueWaitResult::RetryAfterYield);
            }
            drop(context);

            match check_state() {
                WorkqueueWaitState::Ready => Poll::Ready(WorkqueueWaitResult::Ready),
                WorkqueueWaitState::Pending => Poll::Pending,
            }
        })) {
            WorkqueueWaitResult::Ready => return WorkqueueWaitResult::Ready,
            WorkqueueWaitResult::RetryAfterYield => yield_now(),
        }
    }
}

fn worker_name(
    kind: kwork::raw::SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
    worker_id: kwork::raw::WorkerId,
) -> String {
    format!(
        "kwork/{}/{}:{}",
        kind.queue_name(),
        cpu_id.as_usize(),
        worker_id.as_usize()
    )
}

fn system_worker_manager_name(
    kind: kwork::raw::SystemWorkQueueKind,
    cpu_id: LogicalCpuId,
) -> String {
    format!("kwork/{}/{}/manager", kind.queue_name(), cpu_id.as_usize())
}

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::vec::Vec;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use kirq::context::{local_bh_disable, test_support::ScopedHardIrqContext};
    use unittest::{assert_eq, def_test};

    use super::*;

    static SYSTEM_WORKER_RUNS: AtomicUsize = AtomicUsize::new(0);
    static LONG_WORK_STARTED: AtomicUsize = AtomicUsize::new(0);
    static LONG_WORK_CAN_FINISH: AtomicUsize = AtomicUsize::new(0);
    static BLOCK_ON_WORK_STARTED: AtomicUsize = AtomicUsize::new(0);
    static BLOCK_ON_WORK_CAN_FINISH: AtomicUsize = AtomicUsize::new(0);
    static FLUSH_FINISHED: AtomicUsize = AtomicUsize::new(0);
    static FLUSH_RESULT: AtomicUsize = AtomicUsize::new(0);
    static CANCEL_FINISHED: AtomicUsize = AtomicUsize::new(0);
    static CANCEL_STARTED: AtomicUsize = AtomicUsize::new(0);
    static REQUEUE_RESULT: AtomicUsize = AtomicUsize::new(0);
    static SELF_WAIT_RESULT: AtomicUsize = AtomicUsize::new(0);

    struct ScopedRemovedSystemPool {
        kind: kwork::raw::SystemWorkQueueKind,
        cpu_id: LogicalCpuId,
        previous: Option<SystemPoolWakeSourcesEntry>,
    }

    impl ScopedRemovedSystemPool {
        fn remove(kind: kwork::raw::SystemWorkQueueKind, cpu_id: LogicalCpuId) -> Option<Self> {
            let slot = SYSTEM_POOL_WAKE_SOURCES
                .0
                .get(kind.as_usize())?
                .get(cpu_id.as_usize())?;
            let previous = slot.lock().take();
            Some(Self {
                kind,
                cpu_id,
                previous,
            })
        }
    }

    impl Drop for ScopedRemovedSystemPool {
        fn drop(&mut self) {
            if let Some(slot) = SYSTEM_POOL_WAKE_SOURCES
                .0
                .get(self.kind.as_usize())
                .and_then(|slots| slots.get(self.cpu_id.as_usize()))
            {
                *slot.lock() = self.previous.take();
            }
        }
    }

    fn count_system_work(_work: &kwork::raw::ScheduledWork) {
        SYSTEM_WORKER_RUNS.fetch_add(1, Ordering::Release);
    }

    fn long_system_work(_work: &kwork::raw::ScheduledWork) {
        LONG_WORK_STARTED.store(1, Ordering::Release);
        while LONG_WORK_CAN_FINISH.load(Ordering::Acquire) == 0 {
            yield_now();
        }
    }

    fn blocking_system_work(wait_source: PollSet) -> kwork::raw::ScheduledWork {
        kwork::raw::ScheduledWork::new(move |_work| {
            let mut registrations = PollRegistrations::new();
            block_on(poll_fn(|cx| {
                if BLOCK_ON_WORK_CAN_FINISH.load(Ordering::Acquire) != 0 {
                    return Poll::Ready(());
                }

                let mut context = registrations.context(cx);
                context
                    .register(&wait_source)
                    .expect("test wait registration should succeed");
                drop(context);
                BLOCK_ON_WORK_STARTED.store(1, Ordering::Release);

                if BLOCK_ON_WORK_CAN_FINISH.load(Ordering::Acquire) != 0 {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }));
        })
    }

    fn requeue_then_wait_system_work(work: &kwork::raw::ScheduledWork) {
        LONG_WORK_STARTED.store(1, Ordering::Release);
        let code = match kwork::raw::schedule_work(work) {
            kwork::raw::QueueWorkResult::Queued => 1,
            kwork::raw::QueueWorkResult::AlreadyQueued => 2,
            kwork::raw::QueueWorkResult::Disabled => 3,
            kwork::raw::QueueWorkResult::QueueFull => 4,
            kwork::raw::QueueWorkResult::InvalidCpu => 5,
            kwork::raw::QueueWorkResult::WorkerUnavailable => 6,
        };
        REQUEUE_RESULT.store(code, Ordering::Release);
        while LONG_WORK_CAN_FINISH.load(Ordering::Acquire) == 0 {
            yield_now();
        }
    }

    fn queue_then_wait_dynamic_work(
        queue: kwork::raw::WorkQueueHandle,
    ) -> kwork::raw::ScheduledWork {
        kwork::raw::ScheduledWork::new(move |work| {
            LONG_WORK_STARTED.store(1, Ordering::Release);
            while REQUEUE_RESULT.load(Ordering::Acquire) == 0 {
                yield_now();
            }
            if REQUEUE_RESULT
                .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
                && queue.queue_work(work) != kwork::raw::QueueWorkResult::AlreadyQueued
            {
                panic!("running work should reject a second queue attempt");
            }
            while LONG_WORK_CAN_FINISH.load(Ordering::Acquire) == 0 {
                yield_now();
            }
        })
    }

    fn self_flush_work(work: &kwork::raw::ScheduledWork) {
        let result = work.flush();
        let code = match result {
            Err(kwork::raw::WorkqueueError::SelfWait) => 2,
            Ok(_) => 1,
            Err(_) => 3,
        };
        SELF_WAIT_RESULT.store(code, Ordering::Release);
    }

    fn flush_result_code(result: Result<bool, kwork::raw::WorkqueueError>) -> usize {
        match result {
            Ok(false) => 1,
            Ok(true) => 2,
            Err(kwork::raw::WorkqueueError::SelfWait) => 3,
            Err(kwork::raw::WorkqueueError::InvalidContext) => 4,
            Err(kwork::raw::WorkqueueError::WaitFailed) => 5,
            Err(kwork::raw::WorkqueueError::QueueFailed) => 6,
            Err(kwork::raw::WorkqueueError::WorkerUnavailable) => 7,
            Err(kwork::raw::WorkqueueError::UnsupportedQueue) => 8,
            Err(kwork::raw::WorkqueueError::BarrierFull) => 9,
        }
    }

    #[def_test]
    fn test_worker_name_includes_cpu_id() {
        assert_eq!(
            worker_name(
                kwork::raw::SystemWorkQueueKind::Default,
                LogicalCpuId::new(3),
                kwork::raw::WorkerId::new(1)
            ),
            String::from("kwork/system_wq/3:1")
        );
        assert_eq!(
            system_worker_manager_name(
                kwork::raw::SystemWorkQueueKind::Default,
                LogicalCpuId::new(3)
            ),
            String::from("kwork/system_wq/3/manager")
        );
    }

    #[def_test(serial)]
    fn test_system_wq_enqueue_rejects_unavailable_worker_pool() {
        init_system_workqueue_worker();
        let cpu_id = khal::percpu::this_cpu_id();
        let queue =
            kwork::raw::system_percpu_wq_for_cpu(cpu_id).expect("current CPU queue should exist");
        let _removed =
            ScopedRemovedSystemPool::remove(kwork::raw::SystemWorkQueueKind::Default, cpu_id)
                .expect("current CPU worker pool slot should be in range");
        let work = kwork::raw::ScheduledWork::new(count_system_work);
        let direct_work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(
            kwork::raw::schedule_work_on(cpu_id, &work),
            kwork::raw::QueueWorkResult::WorkerUnavailable
        );
        assert_eq!(
            queue.queue_work(&direct_work),
            kwork::raw::QueueWorkResult::WorkerUnavailable
        );
    }

    #[def_test(serial)]
    fn test_long_system_wq_enqueue_rejects_unavailable_worker_pool() {
        init_system_workqueue_worker();
        let cpu_id = khal::percpu::this_cpu_id();
        let queue = kwork::raw::system_long_wq_for_cpu(cpu_id)
            .expect("current CPU long queue should exist");
        let _removed =
            ScopedRemovedSystemPool::remove(kwork::raw::SystemWorkQueueKind::Default, cpu_id)
                .expect("current CPU default worker pool slot should be in range");
        let work = kwork::raw::ScheduledWork::new(count_system_work);
        let direct_work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(
            kwork::raw::schedule_long_work_on(cpu_id, &work),
            kwork::raw::QueueWorkResult::WorkerUnavailable
        );
        assert_eq!(
            queue.queue_work(&direct_work),
            kwork::raw::QueueWorkResult::WorkerUnavailable
        );
    }

    #[def_test(serial)]
    fn test_system_worker_drains_scheduled_work() {
        init_system_workqueue_worker();

        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(
            kwork::raw::schedule_work(&work),
            kwork::raw::QueueWorkResult::Queued
        );
        work.flush().expect("system work should drain");

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
    }

    #[def_test(serial)]
    fn test_long_system_worker_drains_scheduled_work() {
        init_system_workqueue_worker();

        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(
            kwork::raw::schedule_long_work(&work),
            kwork::raw::QueueWorkResult::Queued
        );
        work.flush().expect("long system work should drain");

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
    }

    #[def_test(serial)]
    fn test_system_worker_drains_flushed_delayed_work() {
        init_system_workqueue_worker();

        let cpu_id = khal::percpu::this_cpu_id();
        let queue =
            kwork::raw::system_percpu_wq_for_cpu(cpu_id).expect("current CPU queue should exist");
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let work = kwork::raw::DelayedScheduledWork::new(count_system_work);

        assert_eq!(
            queue.queue_delayed_work(&work, ktime_types::TimeSpan::from_secs(60)),
            kwork::raw::QueueDelayedWorkResult::Queued
        );
        work.flush().expect("system delayed work should flush");

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
    }

    #[def_test(serial)]
    fn test_dynamic_workqueue_worker_drains_queued_work() {
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let queue =
            kwork::raw::WorkQueueHandle::alloc("test-dynamic", kwork::raw::WorkQueueAttrs::new())
                .expect("dynamic workqueue allocation should succeed");
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(queue.queue_work(&work), kwork::raw::QueueWorkResult::Queued);
        work.flush().expect("dynamic work should drain");

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
        queue.destroy().expect("dynamic workqueue should destroy");
    }

    #[def_test(serial)]
    fn test_dynamic_workqueue_enqueue_is_allowed_in_hardirq_context_after_pool_ready() {
        init_system_workqueue_worker();
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let queue = kwork::raw::WorkQueueHandle::alloc(
            "test-dynamic-hardirq-enqueue",
            kwork::raw::WorkQueueAttrs::new(),
        )
        .expect("dynamic workqueue allocation should succeed");
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        {
            let _hardirq = ScopedHardIrqContext::enter();
            assert_eq!(queue.queue_work(&work), kwork::raw::QueueWorkResult::Queued);
        }

        work.flush().expect("hardirq-queued work should drain");
        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
        queue.destroy().expect("dynamic workqueue should destroy");
    }

    #[def_test(serial)]
    fn test_dynamic_workqueue_enqueue_is_allowed_with_bh_disabled_after_pool_ready() {
        init_system_workqueue_worker();
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let queue = kwork::raw::WorkQueueHandle::alloc(
            "test-dynamic-bh-disabled-enqueue",
            kwork::raw::WorkQueueAttrs::new(),
        )
        .expect("dynamic workqueue allocation should succeed");
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        {
            let _bh = local_bh_disable();
            assert_eq!(queue.queue_work(&work), kwork::raw::QueueWorkResult::Queued);
        }

        work.flush().expect("BH-disabled queued work should drain");
        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
        queue.destroy().expect("dynamic workqueue should destroy");
    }

    #[def_test(serial)]
    fn test_dynamic_workqueue_flush_waits_for_queued_work() {
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let queue = kwork::raw::WorkQueueHandle::alloc(
            "test-dynamic-flush",
            kwork::raw::WorkQueueAttrs::new(),
        )
        .expect("dynamic workqueue allocation should succeed");
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(queue.queue_work(&work), kwork::raw::QueueWorkResult::Queued);
        queue.flush().expect("dynamic queue flush should wait");

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
        queue.destroy().expect("dynamic workqueue should destroy");
    }

    #[def_test(serial)]
    fn test_dynamic_workqueue_flush_delayed_work_queues_immediately() {
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let queue = kwork::raw::WorkQueueHandle::alloc(
            "test-dynamic-delay",
            kwork::raw::WorkQueueAttrs::new(),
        )
        .expect("dynamic workqueue allocation should succeed");
        let work = kwork::raw::DelayedScheduledWork::new(count_system_work);

        assert_eq!(
            queue.queue_delayed_work(&work, ktime_types::TimeSpan::from_secs(60)),
            kwork::raw::QueueDelayedWorkResult::Queued
        );
        work.flush().expect("delayed work should flush");

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
        queue.destroy().expect("dynamic workqueue should destroy");
    }

    #[def_test(serial)]
    fn test_dynamic_workqueue_destroy_rejects_late_queue() {
        let queue = kwork::raw::WorkQueueHandle::alloc(
            "test-dynamic-destroy",
            kwork::raw::WorkQueueAttrs::new(),
        )
        .expect("dynamic workqueue allocation should succeed");
        let clone = queue.clone();
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        queue.destroy().expect("dynamic workqueue should destroy");

        assert_eq!(
            clone.queue_work(&work),
            kwork::raw::QueueWorkResult::Disabled
        );
    }

    #[def_test(serial)]
    fn test_destroy_dynamic_workqueue_drains_pending_work() {
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let queue = kwork::raw::WorkQueueHandle::alloc(
            "test-dynamic-drain-destroy",
            kwork::raw::WorkQueueAttrs::new(),
        )
        .expect("dynamic workqueue allocation should succeed");
        let clone = queue.clone();
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(queue.queue_work(&work), kwork::raw::QueueWorkResult::Queued);
        queue.destroy().expect("dynamic workqueue should destroy");

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
        assert_eq!(
            clone.queue_work(&work),
            kwork::raw::QueueWorkResult::Disabled
        );
    }

    #[def_test(serial)]
    fn test_system_pool_drains_other_work_after_yielding_worker_becomes_cpu_intensive() {
        init_system_workqueue_worker();

        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);

        let long_work = kwork::raw::ScheduledWork::new(long_system_work);
        let other_work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(
            kwork::raw::schedule_work(&long_work),
            kwork::raw::QueueWorkResult::Queued
        );
        assert_eq!(
            kwork::raw::schedule_work(&other_work),
            kwork::raw::QueueWorkResult::Queued
        );

        while LONG_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        other_work
            .flush()
            .expect("scheduler tick should mark yielding long work CPU-intensive");

        assert_eq!(LONG_WORK_STARTED.load(Ordering::Acquire), 1);
        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);

        LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        long_work.flush().expect("long system work should finish");
    }

    #[def_test(serial)]
    fn test_system_pool_drains_other_work_while_one_worker_blocks_on_poll() {
        init_system_workqueue_worker();

        BLOCK_ON_WORK_STARTED.store(0, Ordering::Release);
        BLOCK_ON_WORK_CAN_FINISH.store(0, Ordering::Release);
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let wait_source = PollSet::new();

        let blocking_work = blocking_system_work(wait_source.clone());
        let other_work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(
            kwork::raw::schedule_work(&blocking_work),
            kwork::raw::QueueWorkResult::Queued
        );
        assert_eq!(
            kwork::raw::schedule_work(&other_work),
            kwork::raw::QueueWorkResult::Queued
        );

        while BLOCK_ON_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        other_work
            .flush()
            .expect("second system worker should drain while first blocks in block_on");

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);

        BLOCK_ON_WORK_CAN_FINISH.store(1, Ordering::Release);
        let _ = wait_source.wake();
        blocking_work
            .flush()
            .expect("blocking system work should finish");
    }

    #[def_test(serial)]
    fn test_schedule_work_on_queues_to_target_system_cpu() {
        init_system_workqueue_worker();
        let cpu_id = khal::percpu::this_cpu_id();

        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(
            kwork::raw::schedule_work_on(cpu_id, &work),
            kwork::raw::QueueWorkResult::Queued
        );
        work.flush().expect("targeted system work should drain");

        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before + 1);
    }

    #[def_test(serial)]
    fn test_flush_work_waits_for_system_worker_callback() {
        init_system_workqueue_worker();

        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        FLUSH_FINISHED.store(0, Ordering::Release);
        clear_workqueue_wait_probe_for_tests();

        let work = kwork::raw::ScheduledWork::new(long_system_work);
        assert_eq!(
            kwork::raw::schedule_work(&work),
            kwork::raw::QueueWorkResult::Queued
        );

        while LONG_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }

        arm_workqueue_wait_probe_for_tests();
        crate::spawn(|| {
            while !workqueue_wait_probe_observed_for_tests() {
                yield_now();
            }
            if FLUSH_FINISHED.load(Ordering::Acquire) != 0 {
                panic!("flush should still be blocked when wait probe fires");
            }
            LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        });

        assert_eq!(
            work.flush(),
            Ok(true),
            "flush_work should wait in task context"
        );
        FLUSH_FINISHED.store(1, Ordering::Release);
        clear_workqueue_wait_probe_for_tests();
    }

    #[def_test(serial)]
    fn test_flush_work_waits_for_running_dynamic_work() {
        let queue = kwork::raw::WorkQueueHandle::alloc("test", kwork::raw::WorkQueueAttrs::new())
            .expect("dynamic workqueue allocation should succeed");

        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        FLUSH_FINISHED.store(0, Ordering::Release);
        clear_workqueue_wait_probe_for_tests();

        let work = kwork::raw::ScheduledWork::new(long_system_work);
        assert_eq!(queue.queue_work(&work), kwork::raw::QueueWorkResult::Queued);

        while LONG_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }

        let flush_work = work.clone();
        arm_workqueue_wait_probe_for_tests();
        crate::spawn(|| {
            while !workqueue_wait_probe_observed_for_tests() {
                yield_now();
            }
            if FLUSH_FINISHED.load(Ordering::Acquire) != 0 {
                panic!("flush should still be blocked when wait probe fires");
            }
            LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        });

        assert_eq!(
            flush_work.flush(),
            Ok(true),
            "flush_work should wait for dynamic work"
        );
        FLUSH_FINISHED.store(1, Ordering::Release);
        clear_workqueue_wait_probe_for_tests();
        queue.destroy().expect("dynamic workqueue should destroy");
    }

    #[def_test(serial)]
    fn test_flush_work_does_not_wait_later_dynamic_queue_attempt() {
        let queue = kwork::raw::WorkQueueHandle::alloc(
            "test-requeue-flush",
            kwork::raw::WorkQueueAttrs::new(),
        )
        .expect("dynamic workqueue allocation should succeed");

        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        FLUSH_FINISHED.store(0, Ordering::Release);
        REQUEUE_RESULT.store(0, Ordering::Release);
        clear_workqueue_wait_probe_for_tests();

        let work = queue_then_wait_dynamic_work(queue.clone());
        assert_eq!(queue.queue_work(&work), kwork::raw::QueueWorkResult::Queued);

        while LONG_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }

        let flush_work = work.clone();
        arm_workqueue_wait_probe_for_tests();
        crate::spawn(|| {
            while !workqueue_wait_probe_observed_for_tests() {
                yield_now();
            }
            if FLUSH_FINISHED.load(Ordering::Acquire) != 0 {
                panic!("flush should still be blocked when wait probe fires");
            }
            REQUEUE_RESULT.store(1, Ordering::Release);
            while REQUEUE_RESULT.load(Ordering::Acquire) != 2 {
                yield_now();
            }
            LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        });

        assert_eq!(
            flush_work.flush(),
            Ok(true),
            "flush_work should wait only for the observed running instance"
        );
        FLUSH_FINISHED.store(1, Ordering::Release);
        assert_eq!(REQUEUE_RESULT.load(Ordering::Acquire), 2);
        clear_workqueue_wait_probe_for_tests();
        queue.destroy().expect("dynamic workqueue should destroy");
    }

    #[def_test(serial)]
    fn test_flush_work_waits_for_pending_dynamic_work_when_queue_is_full() {
        let queue = kwork::raw::WorkQueueHandle::alloc(
            "test-full-flush",
            kwork::raw::WorkQueueAttrs::new().with_max_active(1),
        )
        .expect("dynamic workqueue should allocate");
        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let target = kwork::raw::ScheduledWork::new(long_system_work);
        let mut fillers = Vec::new();

        assert_eq!(
            queue.queue_work(&target),
            kwork::raw::QueueWorkResult::Queued
        );
        while LONG_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        for _ in 1..kwork::raw::MAX_WORKQUEUE_PENDING {
            let filler = kwork::raw::ScheduledWork::new(count_system_work);
            assert_eq!(
                queue.queue_work(&filler),
                kwork::raw::QueueWorkResult::Queued
            );
            fillers.push(filler);
        }

        FLUSH_RESULT.store(0, Ordering::Release);
        clear_workqueue_wait_probe_for_tests();
        arm_workqueue_wait_probe_for_tests();
        let flush_target = target.clone();
        crate::spawn(move || {
            FLUSH_RESULT.store(flush_result_code(flush_target.flush()), Ordering::Release);
        });

        while !workqueue_wait_probe_observed_for_tests() {
            yield_now();
        }
        assert_eq!(FLUSH_RESULT.load(Ordering::Acquire), 0);

        LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        while FLUSH_RESULT.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        assert_eq!(FLUSH_RESULT.load(Ordering::Acquire), 2);
        clear_workqueue_wait_probe_for_tests();
        queue.destroy().expect("dynamic workqueue should destroy");
        assert_eq!(
            SYSTEM_WORKER_RUNS.load(Ordering::Acquire),
            before + fillers.len()
        );
    }

    #[def_test(serial)]
    fn test_cancel_pending_dynamic_work_releases_flush_waiter() {
        let queue = kwork::raw::WorkQueueHandle::alloc(
            "test-cancel-flush",
            kwork::raw::WorkQueueAttrs::new().with_max_active(1),
        )
        .expect("dynamic workqueue should allocate");
        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        let before = SYSTEM_WORKER_RUNS.load(Ordering::Acquire);
        let blocker = kwork::raw::ScheduledWork::new(long_system_work);
        let work = kwork::raw::ScheduledWork::new(count_system_work);

        assert_eq!(
            queue.queue_work(&blocker),
            kwork::raw::QueueWorkResult::Queued
        );
        while LONG_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        assert_eq!(queue.queue_work(&work), kwork::raw::QueueWorkResult::Queued);

        FLUSH_RESULT.store(0, Ordering::Release);
        clear_workqueue_wait_probe_for_tests();
        arm_workqueue_wait_probe_for_tests();
        let flush_work = work.clone();
        crate::spawn(move || {
            FLUSH_RESULT.store(flush_result_code(flush_work.flush()), Ordering::Release);
        });

        while !workqueue_wait_probe_observed_for_tests() {
            yield_now();
        }
        assert_eq!(FLUSH_RESULT.load(Ordering::Acquire), 0);

        assert_eq!(
            work.cancel(),
            kwork::raw::CancelWorkResult::CancelledPending
        );
        while FLUSH_RESULT.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        assert_eq!(FLUSH_RESULT.load(Ordering::Acquire), 2);
        assert_eq!(SYSTEM_WORKER_RUNS.load(Ordering::Acquire), before);
        LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        blocker.flush().expect("blocking work should finish");
        clear_workqueue_wait_probe_for_tests();
        queue.destroy().expect("dynamic workqueue should destroy");
    }

    #[def_test(serial)]
    fn test_cancel_work_sync_waits_for_running_system_work() {
        init_system_workqueue_worker();

        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        CANCEL_FINISHED.store(0, Ordering::Release);
        CANCEL_STARTED.store(0, Ordering::Release);

        let work = kwork::raw::ScheduledWork::new(long_system_work);
        assert_eq!(
            kwork::raw::schedule_work(&work),
            kwork::raw::QueueWorkResult::Queued
        );

        while LONG_WORK_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }

        let cancel_work = work.clone();
        crate::spawn(move || {
            CANCEL_STARTED.store(1, Ordering::Release);
            cancel_work
                .cancel_sync()
                .expect("cancel_work_sync should wait in task context");
            CANCEL_FINISHED.store(1, Ordering::Release);
        });

        while CANCEL_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        assert_eq!(CANCEL_FINISHED.load(Ordering::Acquire), 0);

        LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        while CANCEL_FINISHED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
    }

    #[def_test(serial)]
    fn test_cancel_work_sync_blocks_queueing_while_running() {
        init_system_workqueue_worker();

        LONG_WORK_STARTED.store(0, Ordering::Release);
        LONG_WORK_CAN_FINISH.store(0, Ordering::Release);
        CANCEL_FINISHED.store(0, Ordering::Release);
        CANCEL_STARTED.store(0, Ordering::Release);
        REQUEUE_RESULT.store(0, Ordering::Release);

        let work = kwork::raw::ScheduledWork::new(requeue_then_wait_system_work);
        assert_eq!(
            kwork::raw::schedule_work(&work),
            kwork::raw::QueueWorkResult::Queued
        );

        while REQUEUE_RESULT.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        assert_eq!(REQUEUE_RESULT.load(Ordering::Acquire), 2);

        let cancel_work = work.clone();
        crate::spawn(move || {
            CANCEL_STARTED.store(1, Ordering::Release);
            cancel_work
                .cancel_sync()
                .expect("cancel_work_sync should wait in task context");
            CANCEL_FINISHED.store(1, Ordering::Release);
        });

        while CANCEL_STARTED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        while kwork::raw::schedule_work(&work) != kwork::raw::QueueWorkResult::Disabled {
            yield_now();
        }
        assert_eq!(CANCEL_FINISHED.load(Ordering::Acquire), 0);

        LONG_WORK_CAN_FINISH.store(1, Ordering::Release);
        while CANCEL_FINISHED.load(Ordering::Acquire) == 0 {
            yield_now();
        }
    }

    #[def_test(serial)]
    fn test_flush_work_rejects_self_wait_from_worker_callback() {
        init_system_workqueue_worker();

        SELF_WAIT_RESULT.store(0, Ordering::Release);
        let work = kwork::raw::ScheduledWork::new(self_flush_work);
        assert_eq!(
            kwork::raw::schedule_work(&work),
            kwork::raw::QueueWorkResult::Queued
        );

        while SELF_WAIT_RESULT.load(Ordering::Acquire) == 0 {
            yield_now();
        }
        assert_eq!(SELF_WAIT_RESULT.load(Ordering::Acquire), 2);
    }
}
