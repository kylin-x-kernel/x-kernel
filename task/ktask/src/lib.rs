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

#![allow(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: case (b): legacy ktask has 11 diagnostic \
              sites, too many to judge individually in this migration; new code remains reviewed"
)]
#![cfg_attr(not(test), no_std)]

#[cfg(all(feature = "smp", not(feature = "ipi")))]
compile_error!(
    "ktask's SMP run queues require the `ipi` feature so remote runnable tasks can wake a NOHZ CPU"
);

#[cfg(test)]
mod tests;

#[macro_use]
extern crate log;

extern crate alloc;

// Link the standalone core's test descriptors into the kernel harness.
#[cfg(unittest)]
extern crate ktimer_core;

#[cfg(all(unittest, feature = "smp"))]
mod tests_timer_routing;

#[cfg(unittest)]
mod tests_timer_irq;

#[macro_use]
mod api;
mod irq_wait;
mod rcu;
mod run_queue;
#[cfg(feature = "snapshot")]
pub mod snapshot;
mod softirqd;
mod task;
#[cfg(feature = "snapshot")]
mod task_registry;
mod timers;
mod tracing_hooks;
mod wait_queue;

pub mod future;

pub use self::{
    api::{sleep, sleep_until, yield_now, *},
    rcu::init_service as init_rcu_service,
    softirqd::init_current_cpu as init_softirqd_current_cpu,
    timers::{CancelResult, Expiration, Timer, TimerControl, TimerHandler},
    tracing_hooks::register_sched_trace_hooks,
};
