// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic kernel workerqueue foundation.
//!
//! This crate owns work state, logical workqueues, pool bindings, and worker
//! pool scheduling state; it does not own task creation or blocking. All
//! task-context queues share scheduler-owned per-CPU worker-pool tasks.
//! Bottom-half queues use a separate non-sleepable execution domain.
//!
//! # Usage
//!
//! ## Task-context work
//!
//! Use task-context system work for callbacks that may sleep, allocate, or call
//! ordinary kernel APIs that require process context. The default
//! [`ScheduleAttrs::system`] targets the global system workqueue and resolves a
//! per-CPU pool binding at enqueue time; use [`ScheduleAttrs::long_system`] for
//! long-running callbacks that should not occupy default system workers.
//!
//! [`ScheduledWork::new`] creates an idle work instance. Creation may allocate
//! and must be done before IRQ-like producers use the instance; enqueueing the
//! existing handle does not allocate.
//!
//! ```ignore
//! use kwork::ScheduledWork;
//!
//! let work = ScheduledWork::new(|_work| {
//!     // Runs later in sleepable task context.
//!     rebuild_cache();
//! });
//!
//! // IRQ-safe enqueue of the preallocated instance.
//! work.schedule();
//!
//! // Task context: waiting APIs may sleep.
//! work.cancel_sync()?;
//! ```
//!
//! ## Bottom-half work
//!
//! Use bottom-half work when a producer needs Linux-like `WQ_BH` behavior:
//! quick deferred execution from the softirq-backed, non-sleepable workerqueue
//! domain. Bottom-half callbacks must not sleep, block, flush work, call
//! [`ScheduledWork::cancel_sync`], destroy queues, or call APIs that may wait.
//!
//! The [`ScheduledWork`] instance must be created before entering IRQ-like
//! context. Interrupt handlers, serving-softirq code, and BH-disabled sections
//! only enqueue the preallocated instance with
//! [`ScheduledWork::schedule`], which does not allocate.
//!
//! ```ignore
//! use kwork::{QueueWorkResult, ScheduleAttrs, ScheduledWork};
//!
//! // Init/task context: create the template and preallocate the schedule
//! // instance. Use `bottom_half_highpri()` for the high-priority BH lane.
//! let rx_scheduled = ScheduledWork::new(|_work| {
//!     // Runs from softirq context.
//!     drain_rx_budget_irq_safe();
//! });
//!
//! // IRQ, serving-softirq, BH-disabled, or task context: enqueue only.
//! if rx_scheduled.schedule_with(ScheduleAttrs::bottom_half().on_cpu(device_cpu))
//!     == QueueWorkResult::Queued
//! {
//!     mask_or_ack_device_rx_irq();
//! }
//!
//! // Sleepable task context: stop producers, then synchronize.
//! stop_irq_or_device_producers();
//! rx_scheduled.cancel_sync()?;
//! ```
//!
//! Use a dynamic workqueue when a subsystem or driver needs its own active
//! limit, flush accounting, or destroy boundary. Dynamic queues do not own
//! worker tasks; they attach to the same shared per-CPU pools as system work.
//!
//! ```ignore
//! use kwork::{QueueWorkResult, ScheduledWork, WorkQueueAttrs, WorkQueueHandle};
//!
//! let queue = WorkQueueHandle::alloc("net-reset", WorkQueueAttrs::new())?;
//! let scheduled = ScheduledWork::new(|_work| {
//!     // Runs on a shared worker-pool task with this queue's accounting.
//! });
//!
//! if scheduled.schedule_on_queue(&queue) != QueueWorkResult::Queued {
//!     // Handle QueueFull, Disabled, WorkerUnavailable, etc.
//! }
//!
//! // Release order: stop new producers, cancel or flush all work, then destroy
//! // the queue. A ScheduledWork keeps only its instance alive; external device
//! // state captured by the callback is protected by this teardown order.
//! stop_irq_or_device_producers();
//! scheduled.cancel_sync()?;
//! queue.destroy()?;
//! ```
//!
//! Use [`BudgetedPoller`] for NAPI-like producer/consumer paths where
//! IRQ-adjacent producers only publish work and a task-context owner drains a
//! bounded number of units per round.
//!
//! ```ignore
//! let poller = kwork::BudgetedPoller::new(
//!     "net-poller",
//!     background_budget,
//!     assist_budget,
//!     4,
//!     |budget| poll_network_once(budget),
//! );
//! poller.start()?;
//! let _ = poller.notify_irq_safe();
//! ```
//!
//! `WorkQueueAttrs` carries Linux-like `flags` and `max_active` fields. Custom
//! and dynamic queues support explicit `max_active` active throttling;
//! unsupported flags are rejected so callers do not accidentally rely on
//! semantics that are not implemented yet. In particular, [`WorkQueueFlags::BH`]
//! is not a dynamic-queue switch; bottom-half work must use
//! [`ScheduleAttrs::bottom_half`] or [`ScheduleAttrs::bottom_half_highpri`] until
//! custom `WQ_BH` allocation is implemented.
//!
//! Use [`DelayedScheduledWork::new`] for timer-triggered work. The delay and
//! queue target are supplied per schedule operation. The resolved delayed
//! target decides where the inner [`ScheduledWork`] is queued when the timer
//! expires.
//!
//! ```ignore
//! use ktime_types::TimeSpan;
//! use kwork::{DelayedScheduledWork, ScheduleAttrs};
//!
//! let delayed = DelayedScheduledWork::new(|_work| {
//!     refresh_watchdog_timestamp();
//! });
//! delayed.schedule_after_with(
//!     TimeSpan::from_millis(10),
//!     ScheduleAttrs::long_system().on_cpu(cpu_id),
//! );
//!
//! delayed.mod_schedule_after_with(
//!     TimeSpan::from_millis(20),
//!     ScheduleAttrs::long_system().on_cpu(cpu_id),
//! );
//! delayed.cancel_sync()?;
//! ```
//!
//! The preallocated-instance enqueue APIs (`ScheduledWork::schedule`,
//! `queue_work`, and delayed zero-delay enqueue) are non-blocking and are valid
//! from interrupt-like context. Allocation helpers such as
//! [`ScheduledWork::new`] and [`DelayedScheduledWork::new`] are
//! task/init-context conveniences. The
//! synchronous APIs (`flush`, `cancel_sync`, `WorkQueue::flush`, and
//! `WorkQueueHandle::destroy`) may sleep and must be called only from sleepable
//! task context. Do not use delayed work as a replacement for softirq polling;
//! use it when the operation is intentionally timer-triggered and belongs in
//! task context.

#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate log;
extern crate alloc;

mod budgeted_poller;
mod control;
mod healthcheck;
mod pool;
mod provider;
mod queue;
mod runtime;
mod work;
mod wq_pool;

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests;

pub use budgeted_poller::{BudgetedPollProgress, BudgetedPoller, BudgetedPollerStartError};
#[cfg(unittest)]
pub(crate) use control::{CurrentWorkGuard, finish_work};
pub(crate) use control::{
    DeferredWake, FlushTarget, PendingCancel, attach_flush_barrier, cancel_pending_from_binding,
    finish_workqueue_pool_enqueue, process_one_pool_work, queue_result_to_wait_error,
    reject_delayed_target_wait_deadlock, reject_invalid_wait_context, reject_self_wait,
    reject_worker_pool_wait_deadlock, reject_worker_wait_deadlock, wait_for_workqueue_idle,
};
pub use healthcheck::system_workqueue_watchdog_check;
#[cfg(unittest)]
pub(crate) use pool::WorkerState;
pub use pool::{
    WORKER_CREATE_RETRY_DELAY, WorkerExecutionToken, WorkerId, WorkerPoolAttrs,
    WorkerPoolCpuAffinity, WorkerPoolExecution, WorkerPoolSchedulingPolicy, WorkerWakePlan,
};
pub(crate) use pool::{WorkerPool, WorkerPoolStatsSnapshot, WorkerSleepTransition};
pub use provider::{
    WorkqueueBottomHalfIf, WorkqueueContextIf, WorkqueueHostIf, WorkqueueSyncWaitIf,
    WorkqueueTaskContext, WorkqueueTaskContextIf, WorkqueueTimerHandle, WorkqueueTimerIf,
};
pub use runtime::{
    BottomHalfPoolBinding, BottomHalfWorkQueueKind, INITIAL_SYSTEM_WORKERS_PER_CPU,
    MAX_SYSTEM_WORKERS_PER_CPU, SystemPoolBinding, SystemPoolKind, SystemWorkQueueKind,
    schedule_long_work, schedule_long_work_on, schedule_work, schedule_work_on,
    system_bh_highpri_wq, system_bh_wq, system_long_wq, system_percpu_wq, system_wq,
};
pub(crate) use runtime::{
    BottomHalfWake, TaskPoolBinding, TaskPoolWake, bh_queue_cpu_is_valid, bh_wq_kind,
    is_builtin_queue, system_queue_cpu_is_valid,
};
#[cfg(unittest)]
pub(crate) use work::{DelayedFireOutcome, DelayedWorkStatus, clear_delayed_reservation};
pub(crate) use work::{
    DelayedWorkTarget, mod_delayed_work_for_target, queue_delayed_work_for_target,
};
pub(crate) use wq_pool::{
    WorkQueuePoolBarrierAttach, WorkQueuePoolBinding, WorkQueuePoolEnqueue,
    WorkQueuePoolPendingCancel, WorkQueuePoolPendingCancelDone, WorkQueuePoolRunnableTake,
    WorkQueuePoolState, WorkQueueRuntime,
};

pub(crate) use crate::{
    queue::{
        BarrierAttachResult, PendingBarrierAttach, PendingWorkEntry, PendingWorkStore,
        QueueInstanceCompletion, QueueOwner, QueueWake, WorkEntry,
    },
    work::{
        RunQueueEntryClaim, WorkBarrier, WorkBarrierQueue, WorkColor, WorkInstanceId, WorkState,
        WorkStatus, complete_barriers_defer_wake,
    },
};
pub use crate::{
    queue::{
        DEFAULT_WORKQUEUE_MAX_ACTIVE, MAX_WORKQUEUE_PENDING, QueueDelayedWorkResult,
        QueueWorkResult, WorkQueue, WorkQueueAllocError, WorkQueueAttrs, WorkQueueFlags,
        WorkQueueHandle, WorkQueueMaxActive, WorkQueueStartError,
    },
    work::{
        CancelWorkResult, DelayedScheduledWork, ScheduleAttrs, ScheduleQueueRef, ScheduledWork,
        WorkqueueError,
    },
};
#[cfg(unittest)]
pub(crate) use crate::{
    queue::{QueueColorFlush, prepare_queue_color_flush},
    work::MAX_WORK_BARRIERS_PER_SLOT,
};

/// Low-level workerqueue runtime surface used by scheduler and interrupt glue.
///
/// These items are also re-exported at the crate root for the current callback
/// workqueue API. The hidden module remains for provider crates that want an
/// explicit import boundary for scheduler and interrupt glue.
#[doc(hidden)]
pub mod raw {
    pub use crate::{
        control::process_one_bottom_half_pool_work,
        healthcheck::system_workqueue_watchdog_check,
        pool::{
            WORKER_CREATE_RETRY_DELAY, WorkerExecutionToken, WorkerId, WorkerPoolAttrs,
            WorkerPoolCpuAffinity, WorkerPoolExecution, WorkerPoolSchedulingPolicy, WorkerWakePlan,
        },
        provider::{
            WorkqueueBottomHalfIf, WorkqueueContextIf, WorkqueueHostIf, WorkqueueSyncWaitIf,
            WorkqueueTaskContext, WorkqueueTaskContextIf, WorkqueueTimerHandle, WorkqueueTimerIf,
        },
        queue::{
            DEFAULT_WORKQUEUE_MAX_ACTIVE, MAX_WORKQUEUE_PENDING, QueueDelayedWorkResult,
            QueueWorkResult, WorkQueue, WorkQueueAllocError, WorkQueueAttrs, WorkQueueFlags,
            WorkQueueHandle, WorkQueueMaxActive, WorkQueueStartError,
        },
        runtime::{
            BottomHalfPoolBinding, BottomHalfWorkQueueKind, INITIAL_SYSTEM_WORKERS_PER_CPU,
            MAX_SYSTEM_WORKERS_PER_CPU, SystemPoolBinding, SystemPoolKind, SystemWorkQueueKind,
            schedule_long_work, schedule_long_work_on, schedule_work, schedule_work_on,
            system_bh_highpri_wq, system_bh_wq, system_long_wq, system_percpu_wq, system_wq,
        },
        work::{
            CancelWorkResult, DelayedScheduledWork, ScheduleAttrs, ScheduleQueueRef, ScheduledWork,
            WorkqueueError,
        },
    };
}
