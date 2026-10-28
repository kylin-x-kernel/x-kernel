// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;
use unittest::def_test;

#[def_test]
fn freeze_avoids_counter_reads_and_resume_discards_counter_changes() {
    for resume_counter in [0, 100, 500, 1_000_000, u64::MAX - 10] {
        let clock = ClockEpoch::new();
        assert_eq!(clock.read(|| 500), (500, false));
        let token = clock.suspend(|| 500).unwrap();
        assert_eq!(clock.read(|| panic!("frozen clock sampled hardware")), (500, true));
        clock.resume(token, || resume_counter).unwrap();
        assert_eq!(clock.read(|| resume_counter), (500, false));
        assert_eq!(clock.read(|| resume_counter.wrapping_add(20)), (520, false));
    }
}

#[def_test]
fn repeated_suspend_preserves_elapsed_runtime_only() {
    let clock = ClockEpoch::new();
    let first = clock.suspend(|| 100).unwrap();
    clock.resume(first, || 10_000).unwrap();
    let second = clock.suspend(|| 10_050).unwrap();
    assert_eq!(clock.read(|| 0), (150, true));
    clock.resume(second, || 0).unwrap();
    assert_eq!(clock.read(|| 75), (225, false));
}

#[def_test]
fn stale_and_duplicate_transitions_preserve_current_owner() {
    let clock = ClockEpoch::new();
    assert_eq!(clock.resume(0, || panic!("stale sampled hardware")), Err(MonotonicSuspendError::StaleToken));
    let first = clock.suspend(|| 100).unwrap();
    assert_eq!(clock.suspend(|| panic!("duplicate sampled hardware")), Err(MonotonicSuspendError::AlreadySuspended));
    clock.resume(first, || 0).unwrap();
    assert_eq!(clock.resume(first, || 0), Err(MonotonicSuspendError::StaleToken));
    let second = clock.suspend(|| 10).unwrap();
    assert_eq!(clock.resume(first, || 999), Err(MonotonicSuspendError::StaleToken));
    assert_eq!(clock.read(|| 999), (110, true));
    clock.resume(second, || 20).unwrap();
    assert_eq!(clock.read(|| 25), (115, false));
}

#[def_test]
fn read_retries_when_suspend_and_resume_overtake_counter_sample() {
    let clock = ClockEpoch::new();
    let mut reads = 0;
    let reading = clock.read(|| {
        reads += 1;
        if reads == 1 {
            let token = clock.suspend(|| 100).unwrap();
            clock.resume(token, || 10_000).unwrap();
            20
        } else {
            10_010
        }
    });
    assert_eq!(reading, (110, false));
    assert_eq!(reads, 2);
}

#[def_test]
fn transition_identity_exhaustion_always_leaves_room_to_resume() {
    let clock = ClockEpoch::new();
    clock.sequence.store(u64::MAX - 5, Ordering::Relaxed);
    let token = clock.suspend(|| 100).unwrap();
    clock.resume(token, || 50).unwrap();
    assert_eq!(clock.read(|| 60), (110, false));
    assert_eq!(clock.suspend(|| 60), Err(MonotonicSuspendError::IdentityExhausted));
    assert_eq!(clock.read(|| 70), (120, false));
}

#[def_test]
fn competing_writer_is_rejected_without_sampling() {
    let clock = ClockEpoch::new();
    let token = clock.suspend(|| {
        assert_eq!(clock.suspend(|| 0), Err(MonotonicSuspendError::TransitionInProgress));
        100
    }).unwrap();
    clock.resume(token, || 0).unwrap();
}

#[def_test]
fn logical_tick_overflow_saturates_instead_of_wrapping() {
    let clock = ClockEpoch::new();
    let token = clock.suspend(|| u64::MAX - 10).unwrap();
    clock.resume(token, || 0).unwrap();
    assert_eq!(clock.read(|| 20), (u64::MAX, false));
}
