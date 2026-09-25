// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared Work lifetime, per-CPU admission and completion management.
//!
//! Queue management for prepared, caller-owned work. [`Work`] and
//! [`DelayedWork`] expose prepared storage and lifecycle operations; [`service`]
//! exposes queue construction/submission/drain for execution-provider integration. Immutable queue
//! configuration is available through [`QueueConfig`]; it grants no execution
//! capability and does not initialize services. See `docs/design.md` and
//! `docs/security.md` for ownership and context contracts. System-wide service
//! registration and safe owning wrappers belong to the integration layer.
//!
//! Start with [`QueueConfig::new`] and [`service::bind_queue`] after preparing
//! matching execution providers. For example, in sleepable task context, given
//! a live normal queue and with no concurrent producers:
//!
//! ```no_run
//! use alloc::sync::Arc;
//! use core::pin::pin;
//!
//! use kworkqueue_ng::{Work, service::Queue};
//! extern crate alloc;
//!
//! fn run_once(queue: &Arc<Queue>) {
//!     let work = pin!(Work::with_callback(|| {}));
//!     // SAFETY: pinned storage stays live through synchronous cancellation;
//!     // no other producer can access this local work.
//!     unsafe { queue.queue_work(work.as_ref(), None) }.unwrap();
//!     work.flush().unwrap();
//!     work.cancel_sync().unwrap();
//! }
//! ```

#![no_std]

extern crate alloc;

#[cfg(feature = "stall-check")]
mod diagnostics;

mod queue;
mod work;

pub use queue::{CpuPolicy, QueueConfig, QueueConfigError, SubmitError};
pub use work::{
    delayed::{ArmError, DelayedWork, FlushError as DelayedFlushError},
    wait::WaitError,
};

/// Caller-owned ordinary work with one prepared callback and no preparation allocation.
///
/// Create with Work::with_callback, then pin before submission. Queues borrow the
/// object, not its owner: stop and join producers, then successfully cancel_sync
/// before freeing storage or captures. Flush alone is not a teardown barrier.
/// Drop does not wait. The callback must obey its chosen execution context;
/// synchronous waits require sleepable context and must not obstruct callbacks.
/// The internal queue-retention payload is fixed, not caller-provided state.
pub type Work<H = fn()> = work::Work<alloc::sync::Arc<queue::Queue>, H>;

/// One borrowed submission disable; dropping it permits admission when last.
/// This guard does not grant exclusive access or independently keep Work alive.
pub type DisableGuard<'a, H> = work::disable::DisableGuard<'a, alloc::sync::Arc<queue::Queue>, H>;

/// Opaque execution-provider contracts for root service integration, not business APIs.
pub mod service;
