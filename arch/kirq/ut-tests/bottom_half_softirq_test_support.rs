// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::{
    SoftirqAction, SoftirqVec, clear_softirq_diagnostics, clear_softirq_pending_for_tests,
    replace_softirq_action_for_tests, set_softirq_daemon_wake_enabled_for_tests,
};

pub struct ScopedSoftirqAction {
    vec: SoftirqVec,
    previous: Option<SoftirqAction>,
}

impl ScopedSoftirqAction {
    pub fn install(vec: SoftirqVec, action: SoftirqAction) -> Self {
        Self::replace(vec, Some(action))
    }

    pub fn clear(vec: SoftirqVec) -> Self {
        Self::replace(vec, None)
    }

    fn replace(vec: SoftirqVec, action: Option<SoftirqAction>) -> Self {
        clear_softirq_pending_for_tests(vec);
        let previous = replace_softirq_action_for_tests(vec, action);
        Self { vec, previous }
    }
}

impl Drop for ScopedSoftirqAction {
    fn drop(&mut self) {
        clear_softirq_pending_for_tests(self.vec);
        let _ = replace_softirq_action_for_tests(self.vec, self.previous);
    }
}

pub struct ScopedDaemonWakeGate {
    previous: bool,
}

impl ScopedDaemonWakeGate {
    pub fn disabled() -> Self {
        Self {
            previous: set_softirq_daemon_wake_enabled_for_tests(false),
        }
    }
}

impl Drop for ScopedDaemonWakeGate {
    fn drop(&mut self) {
        let _ = set_softirq_daemon_wake_enabled_for_tests(self.previous);
    }
}

pub fn begin_softirq_test() -> ScopedDaemonWakeGate {
    clear_softirq_diagnostics();
    ScopedDaemonWakeGate::disabled()
}
