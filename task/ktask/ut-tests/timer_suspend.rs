// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicUsize, Ordering};

use ktime_types::{MonotonicInstant, TimeSpan};
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn event_suspend_requires_restoration_before_releasing_deadlines() {
    let mut state = EventState::INITIAL;
    let token = state.begin().unwrap();
    assert_eq!(state.finish(token), Err(TimerSuspendError::HardwareNotRestored));
    assert_eq!(state.phase, Phase::Suspended);
    state.restore(token).unwrap();
    assert_eq!(state.phase, Phase::Restored);
    state.finish(token).unwrap();
    assert_eq!(state.phase, Phase::Running);
}

#[def_test]
fn duplicate_transitions_do_not_release_active_suspension() {
    let mut state = EventState::INITIAL;
    let token = state.begin().unwrap();
    assert_eq!(state.begin(), Err(TimerSuspendError::AlreadySuspended));
    state.restore(token).unwrap();
    assert_eq!(state.restore(token), Err(TimerSuspendError::AlreadyRestored));
    assert_eq!(state.begin(), Err(TimerSuspendError::AlreadySuspended));
    assert_eq!(state.phase, Phase::Restored);
    state.finish(token).unwrap();
    assert_eq!(state.finish(token), Err(TimerSuspendError::StaleToken));
}

#[def_test]
fn old_tokens_cannot_restore_or_release_a_later_cycle() {
    let mut state = EventState::INITIAL;
    let old = state.begin().unwrap();
    state.restore(old).unwrap();
    state.finish(old).unwrap();
    let current = state.begin().unwrap();
    assert_eq!(state.restore(old), Err(TimerSuspendError::StaleToken));
    assert_eq!(state.finish(old), Err(TimerSuspendError::StaleToken));
    assert_eq!(state.phase, Phase::Suspended);
    state.restore(current).unwrap();
    state.finish(current).unwrap();
}

#[def_test]
fn identity_exhaustion_preserves_running_state() {
    let mut state = EventState { generation: u64::MAX, phase: Phase::Running };
    assert_eq!(state.begin(), Err(TimerSuspendError::IdentityExhausted));
    assert_eq!(state.phase, Phase::Running);
    assert_eq!(state.generation, u64::MAX);
}

// Guard cleanup restores the initialized local event even if an assertion
// aborts the fixture. The outer IRQ/preemption guard outlives this value.
struct SuspendedFixture {
    token: LocalTimerSuspend,
    is_finished: bool,
}
impl SuspendedFixture {
    fn new() -> Self {
        Self { token: suspend_local_timer().unwrap(), is_finished: false }
    }
    fn resume(&mut self) {
        self.token.restore_hardware().unwrap();
        self.token.resume().unwrap();
        self.is_finished = true;
    }
}
impl Drop for SuspendedFixture {
    fn drop(&mut self) {
        if !self.is_finished {
            // Restoration may already have succeeded before a failed assertion.
            let _ = self.token.restore_hardware();
            let _ = self.token.resume();
        }
    }
}

#[def_test]
fn public_session_rejects_duplicates_and_supports_abort_without_sleep() {
    let guard = NoPreemptIrqSave::new();
    let mut fixture = SuspendedFixture::new();
    assert!(is_suspended(&guard));
    assert_eq!(suspend_local_timer().unwrap_err(), TimerSuspendError::AlreadySuspended);
    assert_eq!(fixture.token.resume(), Err(TimerSuspendError::HardwareNotRestored));
    fixture.token.restore_hardware().unwrap();
    assert!(is_suspended(&guard));
    crate::api::rearm_local_timer();
    assert!(is_suspended(&guard));
    assert_eq!(fixture.token.restore_hardware(), Err(TimerSuspendError::AlreadyRestored));
    fixture.token.resume().unwrap();
    fixture.is_finished = true;
    assert!(!is_suspended(&guard));
    assert_eq!(fixture.token.resume(), Err(TimerSuspendError::StaleToken));
    let mut next = SuspendedFixture::new();
    assert_eq!(fixture.token.restore_hardware(), Err(TimerSuspendError::StaleToken));
    assert!(is_suspended(&guard));
    next.resume();
}

#[derive(Default)]
struct CountExpiry(AtomicUsize);
impl crate::TimerHandler for CountExpiry {
    fn on_expiration(&self, _: crate::TimerControl<'_>, _: crate::Expiration) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn paused_irq_and_resume_never_deliver_overdue_callbacks_inline() {
    let guard = NoPreemptIrqSave::new();
    let timer = crate::Timer::new(CountExpiry::default());
    // SAFETY: boot initialized this CPU's timer service; the guard pins it with
    // IRQs masked. The owned atomic-only handler has stable storage and is
    // cancelled before releasing the guard. Zero is deterministically due.
    unsafe { timer.start(MonotonicInstant::from_span_since_origin(TimeSpan::ZERO)) }.unwrap();
    let mut fixture = SuspendedFixture::new();
    crate::api::rearm_local_timer();
    // The paused branch returns before clock sampling or IRQ-context dispatch.
    crate::api::on_timer_fire();
    assert_eq!(timer.handler().0.load(Ordering::Relaxed), 0);
    assert!(is_suspended(&guard));
    fixture.resume();
    assert_eq!(timer.handler().0.load(Ordering::Relaxed), 0);
    timer.cancel_sync();
    crate::api::rearm_local_timer();
}

#[def_test]
fn timer_wake_batch_cannot_be_suspended_mid_dispatch() {
    let guard = NoPreemptIrqSave::new();
    crate::api::begin_defer_local_timer_rearm();
    let result = suspend_local_timer();
    crate::api::end_defer_local_timer_rearm();
    assert_eq!(result.unwrap_err(), TimerSuspendError::InvalidContext);
    assert!(!is_suspended(&guard));
    crate::api::rearm_local_timer();
}

#[def_test]
fn wrong_cpu_token_cannot_touch_or_release_the_current_event() {
    let guard = NoPreemptIrqSave::new();
    let mut fixture = SuspendedFixture::new();
    let owner = fixture.token.cpu;
    // A different identity suffices: rejection precedes any target-slot access.
    fixture.token.cpu = LogicalCpuId::new(owner.as_usize() ^ 1);
    let restore = fixture.token.restore_hardware();
    let resume = fixture.token.resume();
    fixture.token.cpu = owner;
    assert_eq!(restore, Err(TimerSuspendError::WrongCpu));
    assert_eq!(resume, Err(TimerSuspendError::WrongCpu));
    assert!(is_suspended(&guard));
    fixture.resume();
}
