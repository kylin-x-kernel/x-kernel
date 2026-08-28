// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel workqueue product layer.
//!
//! Queue state lives in `kworkqueue`. Worker scheduling lives in
//! `kworkerpool`. This crate keeps only the callback-facing API and the
//! system queue identities that wire those two cores together.
//!
//! # Built-in queues
//!
//! - [`system_wq`] is the default task-context queue. It chooses a ready
//!   per-CPU normal worker pool for each enqueue and is appropriate for ordinary
//!   sleepable callbacks.
//! - [`system_percpu_wq`] is the CPU-local task-context queue. Enqueue without
//!   an explicit CPU targets the caller's current CPU.
//! - [`system_bh_wq`] and [`system_bh_highpri_wq`] run from bottom-half softirq
//!   drain contexts and must be used only for callbacks that do not sleep.
//!
//! # Basic use
//!
//! ```rust
//! use kwork::{QueueWorkResult, ScheduledWork, system_wq};
//!
//! let work = ScheduledWork::new(|_work| {
//!     // callback body
//! });
//!
//! assert_eq!(system_wq().queue_work(&work), QueueWorkResult::Queued);
//! let waited = work.flush().expect("flush from task context");
//! assert!(waited);
//! ```
//!
//! # Dynamic queue
//!
//! ```rust
//! use kwork::{ScheduledWork, WorkQueueAttrs, WorkQueueHandle};
//!
//! let queue = WorkQueueHandle::alloc("example_wq", WorkQueueAttrs::new().with_max_active(1))
//!     .expect("dynamic queue allocation");
//! let work = ScheduledWork::new(|_| {});
//! let _ = queue.queue_work(&work);
//! queue.flush().expect("flush from task context");
//! queue.destroy().expect("destroy from task context");
//! ```
//!
//! # Delayed work
//!
//! ```rust
//! use ktime_types::TimeSpan;
//! use kwork::{DelayedScheduledWork, ScheduleAttrs};
//!
//! let work = DelayedScheduledWork::new(|_| {});
//! let _ = work.schedule_after_with(TimeSpan::from_millis(10), ScheduleAttrs::system());
//! let _ = work.cancel_sync();
//! ```
//!
//! # Cancellation and blocking rules
//!
//! `flush`, `cancel_sync`, and dynamic queue `destroy` may block and must be
//! called from task context. `cancel` never waits and may be used to remove
//! pending work from IRQ-adjacent producers when the selected queue backend
//! supports that execution context.

#![cfg_attr(not(test), no_std)]

extern crate alloc;

mod budgeted_poller;
mod builtinpool;
mod builtinwq;
mod healthcheck;
mod runtime;
#[cfg(feature = "stress_test")]
mod stress;
mod work;

pub use budgeted_poller::{BudgetedPollProgress, BudgetedPoller, BudgetedPollerStartError};
pub use builtinwq::{system_bh_highpri_wq, system_bh_wq, system_percpu_wq, system_wq};
#[cfg(feature = "stress_test")]
pub use stress::{StressError, StressSummary, run_stress_command, stress_status_text};
pub use work::{
    CancelWorkResult, DEFAULT_WORKQUEUE_MAX_ACTIVE, DelayedScheduledWork, DisableWorkError,
    EnableWorkError, MAX_WORKQUEUE_PENDING, QueueDelayedWorkResult, QueueWorkResult, ScheduleAttrs,
    ScheduledWork, WorkQueue, WorkQueueAllocError, WorkQueueAttrs, WorkQueueFlags, WorkQueueHandle,
    WorkQueueMaxActive, WorkqueueError,
};

/// Low-level workerqueue runtime surface used by startup and watchdog glue.
#[doc(hidden)]
pub mod raw {
    pub use crate::{
        builtinpool::{
            BuiltinBhPoolInitResult, BuiltinCpuPoolInitResult, BuiltinWorkerPoolInitResult,
        },
        healthcheck::system_workqueue_watchdog_check,
        runtime::{init_bottom_half_workerqueue, init_system_workqueue_worker_pools_for_cpu},
    };
}
