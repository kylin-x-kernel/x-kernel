// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! This module provides primitives for task management, including task
//! creation, scheduling, sleeping, termination, etc. The scheduler algorithm
//! is configurable by cargo features.
//!
//! # Cargo Features
//!
//! - `preempt`: Enable preemptive scheduling.
//! - `sched-fifo`: Use the FIFO cooperative scheduler. It also enables the
//! - `sched-rr`: Use the Round-robin preemptive scheduler. It also enables
//!   `preempt` features if it is enabled.
//! - `sched-cfs`: Use the Completely Fair Scheduler. It also enables the
//!   `preempt` features if it is enabled.

#![cfg_attr(not(test), no_std)]

#[cfg(test)]
mod tests;

#[macro_use]
extern crate log;

extern crate alloc;

#[macro_use]
mod run_queue;
mod api;
mod irq_wait;
#[cfg(feature = "snapshot")]
pub mod snapshot;
mod task;
#[cfg(feature = "snapshot")]
mod task_registry;
mod timers;
mod tracing_hooks;
mod wait_queue;

pub mod future;

pub use self::{
    api::{sleep, sleep_until, yield_now, *},
    tracing_hooks::register_sched_trace_hooks,
};
