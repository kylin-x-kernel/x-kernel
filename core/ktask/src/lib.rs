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
//! - `sched-fifo`: Use the [FIFO cooperative scheduler][1]. It also enables the
//! - `sched-rr`: Use the [Round-robin preemptive scheduler][2]. It also enables
//!   `preempt` features if it is enabled.
//! - `sched-cfs`: Use the [Completely Fair Scheduler][3]. It also enables the
//!   `preempt` features if it is enabled.
//!
//! [1]: axsched::FifoScheduler
//! [2]: axsched::RRScheduler
//! [3]: axsched::CFScheduler

#![cfg_attr(not(test), no_std)]
#![feature(doc_cfg)]
#![feature(linkage)]

#[cfg(test)]
mod tests;

#[macro_use]
extern crate log;

extern crate alloc;

#[macro_use]
mod run_queue;
mod api;
#[cfg(feature = "watchdog")]
mod global_task_queue;
mod task;
mod timers;
mod wait_queue;

pub mod future;

pub use self::api::{sleep, sleep_until, yield_now, *};
