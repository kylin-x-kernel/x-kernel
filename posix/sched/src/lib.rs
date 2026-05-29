// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! POSIX scheduling syscall implementations.
//!
//! - Yield control (`sched_yield`)
//! - Sleep operations (`nanosleep`, `clock_nanosleep`)
//! - Scheduling priority (getpriority, setpriority)
//! - CPU affinity (sched_setaffinity, sched_getaffinity)
//! - Scheduler policy (sched_getscheduler, sched_setscheduler, sched_getparam)

#![no_std]

mod affinity;
mod cpu;
mod policy;
mod priority;
mod sleep;
mod yielding;

#[macro_use]
extern crate klogger;

pub use affinity::{sys_sched_getaffinity, sys_sched_setaffinity};
pub use cpu::sys_getcpu;
pub use policy::{sys_sched_getparam, sys_sched_getscheduler, sys_sched_setscheduler};
pub use priority::{sys_getpriority, sys_setpriority};
pub use sleep::{sys_clock_nanosleep, sys_nanosleep};
pub use yielding::sys_sched_yield;
