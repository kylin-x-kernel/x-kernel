// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Workqueue semantic core.
//!
//! This crate contains the queue-side state machine used by higher-level
//! workqueue products. It stores work state, per-CPU queue bindings,
//! active/inactive accounting, flush colors, delayed activation state,
//! cancellation state, and executor operations.
//!
//! It deliberately does not create workers, run callbacks, block tasks, arm
//! timers, or raise interrupts. A runtime layer selects a [`WorkQueueBinding`],
//! calls queue/cancel/flush methods, and applies the returned [`ExecutorOp`]
//! values to its execution backend.
//!
//! # Basic flow
//!
//! ```rust
//! use kcpu_id_map::LogicalCpuId;
//! use kworkqueue::{ClaimResult, ExecutorOp, QueueWorkOutcome, Work, WorkQueue};
//!
//! static QUEUE: WorkQueue<4, 128> = WorkQueue::new("example", 16);
//! static WORK: Work = Work::new();
//!
//! let binding = QUEUE.binding(LogicalCpuId::new(0)).unwrap();
//! let outcome = binding.queue_work(&WORK).unwrap();
//!
//! let entry = match outcome {
//!     QueueWorkOutcome::Runnable(ExecutorOp::EnqueueRunnable(entry)) => entry,
//!     QueueWorkOutcome::Inactive(ExecutorOp::EnqueueInactive(entry)) => entry,
//!     QueueWorkOutcome::QueuedWhileRunning => return,
//! };
//!
//! if let ClaimResult::Run(claimed) = binding.claim(entry, &WORK, 0, 1) {
//!     let _finish = binding.finish(&WORK, claimed);
//! }
//! ```

#![no_std]

extern crate alloc;

mod executor;
mod id;
mod pending;
mod queue;
mod work;

pub use executor::{ExecutorEntry, ExecutorOp};
pub use id::{BindingId, EntryKey, EntryOwner, EntryPayload, WorkColor, WorkInstanceId, WorkKey};
pub use queue::{
    CancelPendingResult, CancelWorkResult, ClaimResult, ClaimedWork, FinishResult, FlushSnapshot,
    QueueWorkError, QueueWorkOutcome, QueueWorkResult, WorkFlushSnapshot, WorkQueue,
    WorkQueueBinding, WorkQueueBindingSnapshot,
};
pub use work::{DisableWorkError, EnableWorkError, Work, WorkStatus};

#[cfg(unittest)]
mod tests;
