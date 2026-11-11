// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Sleeping mutexes with priority inheritance, independent of a scheduler.
//!
//! A task waiting for [`RtMutex`] lends its priority to the owner, and through
//! the owner to whatever that owner waits for. This crate owns that graph:
//! each lock's waiter tree, each task's donor tree, the chain walk, deadlock
//! detection and handoff. The scheduler implements [`PiHost`] to embed a
//! [`PiTaskNode`] in every task, report donation levels and apply boosts.
//! Blocking uses `kiface_task_api::TaskExecutionIf::block_on`.
//!
//! Operations act on the current task. A futex layer that must enqueue another
//! task has to grow a separate explicit-task API; this crate does not provide one.
//!
//! ```ignore
//! // `Host` is the scheduler's `PiHost`; in sleepable task context:
//! static VALUE: krtmutex::RtMutex<Host, u32> = krtmutex::RtMutex::new(0);
//! *VALUE.lock()? += 1;
//! ```

#![no_std]

extern crate alloc;

mod graph;
mod host;
mod mutex;

pub use graph::{PiTaskNode, adjust_waiter_priority};
pub use host::PiHost;
pub use mutex::{RtMutex, RtMutexGuard};
