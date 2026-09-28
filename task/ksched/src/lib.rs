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
mod scheduler;
#[cfg(unittest)]
mod tests;

extern crate alloc;

pub use class::{Decision, EnqueueKind, LeaveReason, UpdateKind};
pub use clock::RqClock;
pub use config::DEFAULT_SLICE_NS;
pub use entity::{KernelClass, SchedTask};
pub use policy::{Nice, SchedAttr, SchedClass, SchedError, SchedPolicy};
pub use scheduler::Scheduler;
