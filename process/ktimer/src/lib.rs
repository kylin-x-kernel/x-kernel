// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-side timer engine and state.

#![no_std]

extern crate alloc;

mod delivery;
mod interval_timer;
mod manager;
mod posix_timer;
mod runtime;

pub use delivery::{TimerDelivery, TimerSignal};
pub use manager::ProcessTimerManager;
pub use posix_timer::{PosixTimerCreateNotify, PosixTimerSigValue, TimerSigValue};
pub use runtime::{register_expired_task_handler, spawn_alarm_task};
