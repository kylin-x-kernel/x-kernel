// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Workqueue object boundary and entry storage.
//!
//! This crate-level module owns the public workqueue object surface, the
//! shared fixed-capacity entry storage primitive used by queue and pool code,
//! and the queue-owned per-CPU `WorkQueuePoolState` array used to bind each
//! queue to the current execution pools. It covers the [`WorkQueue`] /
//! [`WorkQueueHandle`] handles, sync state, queue policy attributes
//! ([`WorkQueueAttrs`]), and the results and errors returned by queue
//! operations. Pending entries live in shared worker pools; this module only
//! provides their bounded storage representation.

mod attrs;
mod entry;
mod entry_queue;
mod error;
mod owner;
mod sync;
mod workqueue;

pub(crate) use attrs::validate_workqueue_attrs;
pub use attrs::{DEFAULT_WORKQUEUE_MAX_ACTIVE, WorkQueueAttrs, WorkQueueFlags, WorkQueueMaxActive};
pub(super) use entry::{BarrierAttachResult, WorkEntry};
pub use entry_queue::MAX_WORKQUEUE_PENDING;
pub(super) use entry_queue::{PendingBarrierAttach, PendingWorkEntry, PendingWorkStore};
pub use error::{
    QueueDelayedWorkResult, QueueWorkResult, WorkQueueAllocError, WorkQueueStartError,
};
pub(super) use owner::QueueOwner;
pub(super) use sync::{
    QueueColorFlush, QueueInstanceCompletion, QueueWake, WorkQueueSyncState,
    prepare_queue_color_flush, wait_for_queue_color_flush,
};
pub use workqueue::{WorkQueue, WorkQueueHandle};
