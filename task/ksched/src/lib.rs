// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![cfg_attr(not(test), no_std)]

mod class;
mod classes;
mod clock;
mod config;
mod entity;
mod policy;
mod ready_list;
mod scheduler;
/// Kernel unit tests for Deadline, RT, Fair, and Idle arbitration.
///
/// Stop stays unwired; FIFO/RR `set_attr` is rejected until the RT class
/// admits them, and Deadline requires a valid `DlParams`.
#[unittest::mod_test]
mod tests {
    include!("tests.rs");
}

extern crate alloc;

pub use class::{Decision, EnqueueKind, LeaveReason, UpdateKind};
pub use clock::RqClock;
pub use config::{
    DEFAULT_RR_SLICE_NS, DEFAULT_RT_PERIOD_NS, DEFAULT_RT_RUNTIME_NS, DEFAULT_SLICE_NS,
};
pub use entity::{KernelClass, SchedTask};
pub use policy::{DlParams, Nice, RtPrio, SchedAttr, SchedClass, SchedError, SchedPolicy};
pub use scheduler::Scheduler;
