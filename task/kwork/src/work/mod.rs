// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Work-layer object model.
//!
//! This crate-level module owns the executor's work object model:
//! the [`ScheduledWork`] refcounted instance handle, its callback and disable
//! gate, the per-instance lifecycle state machine ([`WorkState`] /
//! [`WorkStatus`]) with instance identity and flush colors, per-instance sync
//! barriers, and the delayed-work timer reservation state. The queue, pool,
//! provider, and control layers dispatch operations against scheduled
//! instances.

mod barrier;
mod delayed;
mod error;
mod scheduled_work;
mod state;

#[cfg(unittest)]
pub(crate) use barrier::MAX_WORK_BARRIERS_PER_SLOT;
pub(crate) use barrier::{WorkBarrier, WorkBarrierQueue, complete_barriers_defer_wake};
pub use delayed::DelayedScheduledWork;
#[cfg(unittest)]
pub(crate) use delayed::{DelayedFireOutcome, DelayedWorkStatus, clear_delayed_reservation};
pub(crate) use delayed::{
    DelayedWorkTarget, mod_delayed_work_for_target, queue_delayed_work_for_target,
};
pub use error::{CancelWorkResult, WorkqueueError};
pub use scheduled_work::{ScheduleAttrs, ScheduleQueueRef, ScheduledWork};
pub(crate) use scheduled_work::{ScheduleQueue, ScheduleTarget};
pub(crate) use state::{RunQueueEntryClaim, WorkColor, WorkInstanceId, WorkState, WorkStatus};
