// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Initialization-side ownership of one manager task and its execution service.

use alloc::sync::Arc;
use core::num::NonZeroUsize;

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use ktask::future::PreparedTaskWait;
use ktime_types::TimeSpan;

use super::{NormalService, PartitionWorkSource, WorkerState, manager::WorkerManager};

/// Validation failures before starting or requesting termination of a service.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeError {
    /// The caller cannot block for startup or joined termination.
    InvalidContext,
    /// The requested CPU or complete boot CPU coverage is unavailable.
    CpuUnavailable,
    /// The idle interval is zero or its deadline exceeds the timer range.
    InvalidIdleInterval,
}

/// Rejects intervals whose first idle deadline cannot be represented by the timer.
pub(super) fn validate_idle_interval(idle_for: TimeSpan) -> Result<(), RuntimeError> {
    if idle_for.is_zero()
        || khal::time::monotonic_time()
            .checked_add(idle_for)
            .and_then(|deadline| deadline.span_since_origin().try_as_nanos_u64())
            .is_none()
    {
        return Err(RuntimeError::InvalidIdleInterval);
    }
    Ok(())
}

/// Retains the manager join handle independently of the manager's worker handles.
/// Stop producers and drain/detach sources before stop; retain this owner until
/// stop completes. Drop does not block or stop tasks implicitly. Omitting stop
/// leaves live kernel tasks, not dangling service storage. No global registration
/// or ownership of concrete Work/Queue objects belongs here.
pub(super) struct NormalRuntime<S> {
    pub(super) service: Arc<NormalService<S>>,
    task: ktask::KtaskRef,
}

impl<S: PartitionWorkSource + Send + Sync + 'static> NormalRuntime<S> {
    /// Starts one CPU-bound service after boot CPU/task/timer initialization.
    /// Returns only after manager notification and a baseline worker wait exist.
    /// All recoverable validation precedes service allocation/activation. Allocation and
    /// platform preparation failures follow kernel policy, not rollback errors.
    ///
    /// # Errors
    /// Rejects non-sleepable context, an unmapped CPU, a zero idle interval or
    /// an idle deadline outside the timer's range at preparation time.
    ///
    /// # Panics
    /// Propagates task/timer preparation or initialized runtime invariant failures.
    pub(super) fn start(
        cpu: LogicalCpuId,
        limit: NonZeroUsize,
        idle_for: TimeSpan,
    ) -> Result<Self, RuntimeError> {
        let mut startup = PreparedTaskWait::prepare().map_err(|_| RuntimeError::InvalidContext)?;
        if kcpu_id_map::raw_cpu_id(cpu).is_none() {
            return Err(RuntimeError::CpuUnavailable);
        }
        validate_idle_interval(idle_for)?;
        let manager = WorkerManager::new(cpu, limit, idle_for);
        let service = manager.service.clone();
        let wake = startup.waker();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || manager.run_with_startup(Some(wake)),
            "workng_manager".into(),
            kbuild_config::TASK_STACK_SIZE,
        ));
        task.set_cpumask(KCpuMask::one_shot_logical(cpu));
        ktask::activate_task(&task);
        while !service.is_ready() {
            startup.park();
        }
        Ok(Self { service, task })
    }

    /// Requests whole-service termination and joins manager/timer/worker cleanup.
    /// Idempotent after completion. Caller must exclude producers and finish source
    /// teardown first, and must not call from this manager or one of its workers.
    /// This does not close a Queue or cancel caller-owned delayed work.
    ///
    /// # Errors
    /// Invalid context rejects before publishing the stop request.
    ///
    /// # Panics
    /// Panics on a nonzero manager exit code.
    pub(super) fn stop(&self) -> Result<(), RuntimeError> {
        if !ktask::is_sleepable_context() {
            return Err(RuntimeError::InvalidContext);
        }
        self.service.request_stop();
        assert_eq!(self.task.join(), 0, "normal manager failed");
        Ok(())
    }
}

impl<S> NormalService<S> {
    pub(super) fn is_ready(&self) -> bool {
        let workers = self.workers.lock();
        !workers.stop_requested
            && workers.manager_waker.is_some()
            && workers.wakers.iter().enumerate().any(|(id, wake)| {
                wake.is_some()
                    && matches!(
                        workers.pool.state(id),
                        WorkerState::Available
                            | WorkerState::Selecting
                            | WorkerState::Occupied
                            | WorkerState::Parked { .. }
                    )
            })
    }
}

#[cfg(unittest)]
mod tests;
