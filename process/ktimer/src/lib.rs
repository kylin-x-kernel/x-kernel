// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-side timer engine and state.
//!
//! This crate owns the process-shared `setitimer` state and POSIX
//! interval timers ([`ProcessTimerManager`]), the clock-domain deadline
//! model (`TimerInstant`), and the global alarm queue that wakes timer
//! owners on wall-clock expirations ([`spawn_alarm_task`]). Expiration
//! results are reported as [`TimerDelivery`] values, which `kprocess`
//! converts into `ksignal` notifications. Syscall ABI decoding and signal
//! dispatch live outside this crate.
#![no_std]

extern crate alloc;

mod delivery;
mod interval_timer;
mod manager;
mod posix_timer;
mod runtime;

pub use delivery::{TimerDelivery, TimerSignal};
pub use manager::{PosixTimerUpdate, ProcessTimerManager, RetiredPosixTimer, RetiredPosixTimers};
pub use posix_timer::{
    PosixTimerConfig, PosixTimerCreateError, PosixTimerCreateNotify, PosixTimerSigValue,
    TimerSigValue,
};
pub use posix_types::{Pid, Tid};
pub use runtime::{register_expired_task_handler, spawn_alarm_task};
