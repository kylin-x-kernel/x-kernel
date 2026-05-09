// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread/process timer runtime surface.
//!
//! Timer runtime APIs are grouped here so per-thread CPU accounting and
//! interval-timer management have a dedicated process-side boundary.

pub use ktimer::{TimeManager, TimerState};
use ktypes::Once;

static TIMER_RUNTIME_INIT: Once<()> = Once::new();

/// Installs the timer-expiration bridge and spawns the alarm task once.
pub fn spawn_alarm_task() {
    TIMER_RUNTIME_INIT.call_once(|| {
        ktimer::register_expired_task_handler(crate::poll_timer);
        ktimer::spawn_alarm_task();
    });
}
