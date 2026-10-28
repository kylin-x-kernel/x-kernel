// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{BoottimeInstant, MonotonicInstant};
use unittest::{assert, assert_eq, def_test};

use super::*;

const SECOND: u64 = 1_000_000_000;

fn reading(mono: u64, boot: u64) -> ClockReading {
    ClockReading {
        monotonic: MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(mono)),
        boottime: BoottimeInstant::from_span_since_origin(TimeSpan::from_nanos(boot)),
        realtime: SystemTime::UNIX_EPOCH,
        change_id: Default::default(),
    }
}

fn sample(rtc: i64, mono: u64, boot: u64) -> Sample {
    Sample { rtc: SystemTime::from_unix_seconds(rtc), clocks: reading(mono, boot), read_span: TimeSpan::ZERO }
}

fn policy() -> Policy {
    Policy { resolution: TimeSpan::from_secs(1), max_sleep: TimeSpan::from_secs(3_600), max_read_span: TimeSpan::from_millis(10) }
}

#[derive(Debug)]
struct FakeClock {
    value: Option<SystemTime>,
    resolution: TimeSpan,
    reads: usize,
    changes_resolution: bool,
}
impl FakeClock {
    fn good() -> Self {
        Self { value: Some(SystemTime::from_unix_seconds(1_000)), resolution: TimeSpan::from_secs(1), reads: 0, changes_resolution: false }
    }
}
impl PersistentClock for FakeClock {
    fn read(&mut self) -> Option<SystemTime> {
        self.reads += 1;
        if self.changes_resolution {
            self.resolution = TimeSpan::from_secs(2);
        }
        self.value
    }
    fn resolution(&self) -> TimeSpan { self.resolution }
}

#[def_test]
fn rtc_elapsed_excludes_already_counted_running_time() {
    let before = sample(1_000, 10 * SECOND, 40 * SECOND);
    let after = sample(1_067, 17 * SECOND, 47 * SECOND);
    assert_eq!(estimate(before, after, None, policy()).unwrap().delta, TimeSpan::from_secs(60));
}

#[def_test]
fn wall_clock_edits_do_not_change_sleep_estimate() {
    let first = estimate(
        sample(1_000, 10 * SECOND, 10 * SECOND),
        sample(1_032, 12 * SECOND, 12 * SECOND),
        None,
        policy(),
    ).unwrap();
    assert_eq!(first.delta, TimeSpan::from_secs(30));

    // Keep the previous calibration and a half-second RTC phase difference.
    // An implementation that derives phase from REALTIME would now either
    // perturb compensation or reset calibration on the between-cycle edit.
    let before = sample(1_033, 13 * SECOND + SECOND / 2, 43 * SECOND + SECOND / 2);
    let after = sample(1_065, 15 * SECOND + SECOND / 2, 45 * SECOND + SECOND / 2);
    let expected = estimate(before, after, Some(first.phase), policy()).unwrap();
    assert_eq!(expected.delta, TimeSpan::from_millis(29_500));
    for wall_time in [SystemTime::UNIX_EPOCH, SystemTime::from_unix_seconds(1_033), SystemTime::MAX] {
        let mut edited_before = before;
        let mut edited_after = after;
        edited_before.clocks.realtime = wall_time;
        edited_after.clocks.realtime = SystemTime::MAX;
        let actual = estimate(edited_before, edited_after, Some(first.phase), policy()).unwrap();
        assert_eq!(actual.delta, expected.delta);
        assert_eq!(actual.phase, first.phase);
    }
}

#[def_test]
fn invalid_counter_or_rtc_progress_is_rejected() {
    let before = sample(1_000, 10 * SECOND, 10 * SECOND);
    assert_eq!(estimate(before, sample(999, 11 * SECOND, 11 * SECOND), None, policy()).unwrap_err(), RtcSleepError::RtcWentBackwards);
    assert_eq!(estimate(before, sample(1_001, 9 * SECOND, 9 * SECOND), None, policy()).unwrap_err(), RtcSleepError::InvalidMonotonic);
    assert_eq!(estimate(before, sample(1_000, 12 * SECOND, 12 * SECOND), None, policy()).unwrap_err(), RtcSleepError::InsufficientRtcProgress);
}

#[def_test]
fn excessive_forward_progress_is_bounded_by_pm_policy() {
    assert_eq!(estimate(sample(0, 0, 0), sample(3_601, 0, 0), None, policy()).unwrap_err(), RtcSleepError::SleepTooLong);
    assert_eq!(estimate(sample(0, 0, 0), sample(3_600, 0, 0), None, policy()).unwrap().delta, TimeSpan::from_secs(3_600));
}

#[def_test]
fn subquantum_negative_difference_closes_with_zero_delta() {
    assert_eq!(estimate(sample(0, 0, 0), sample(0, SECOND / 2, SECOND / 2), None, policy()).unwrap().delta, TimeSpan::ZERO);
}

#[def_test]
fn phase_reference_bounds_repeated_fractional_sleep_error() {
    let mut actual = SECOND / 4;
    let mut mono = 0;
    let mut injected = 0;
    let mut naive = 0;
    let mut phase = None;
    for round in 1..=64 {
        actual += 170_000_000;
        mono += 170_000_000;
        let before = sample((actual / SECOND) as i64, mono, mono + injected);
        actual += 110_000_000 + 370_000_000;
        mono += 110_000_000;
        let after = sample((actual / SECOND) as i64, mono, mono + injected);
        let result = estimate(before, after, phase, policy()).unwrap();
        let raw = after.rtc.unix_nanos() - before.rtc.unix_nanos() - 110_000_000;
        naive += raw.max(0);
        phase = Some(result.phase);
        injected += result.delta.as_nanos_u64_saturating();
        let true_sleep = i128::from(round) * 370_000_000;
        assert!((i128::from(injected) - true_sleep).abs() <= i128::from(SECOND));
    }
    assert!((naive - 64 * 370_000_000).abs() > i128::from(SECOND));
}

#[def_test]
fn between_cycle_rtc_adjustment_resets_phase_reference() {
    let before = sample(10_000, SECOND, SECOND);
    let expected = estimate(before, sample(10_010, 2 * SECOND, 2 * SECOND), None, policy()).unwrap();
    let adjusted = estimate(before, sample(10_010, 2 * SECOND, 2 * SECOND), Some(0), policy()).unwrap();
    assert_eq!(adjusted.delta, expected.delta);
    assert_eq!(adjusted.phase, expected.phase);
}

#[def_test]
fn failed_commit_retains_calibration_and_preferred_source_success_clears_it() {
    let mut owner = RtcSleepAccounting::new(FakeClock::good(), TimeSpan::from_secs(60), TimeSpan::from_millis(10)).unwrap();
    owner.phase = Some(17);
    let failure = SuspendAccountingError::SleepTime(super::super::SleepTimeError::OutOfRange);
    assert_eq!(owner.commit_phase(TimeSpan::from_secs(1), None, |_| Err(failure)), Err(RtcSleepError::Accounting(failure)));
    assert_eq!(owner.phase, Some(17));
    owner.commit_phase(TimeSpan::from_secs(1), None, |_| Ok(())).unwrap();
    assert_eq!(owner.phase, None);
    owner.commit_phase(TimeSpan::ZERO, Some(23), |_| Ok(())).unwrap();
    assert_eq!(owner.phase, Some(23));
}

#[def_test]
fn accepted_estimate_advances_boottime_and_realtime_only() {
    let mut keeper = super::super::Timekeeper::INITIAL;
    keeper.set_realtime_checked(SystemTime::from_unix_seconds(1_000), reading(10 * SECOND, 0).monotonic).unwrap();
    let delta = estimate(sample(100, 10 * SECOND, 10 * SECOND), sample(132, 12 * SECOND, 12 * SECOND), None, policy()).unwrap().delta;
    keeper.apply_sleeptime(delta, reading(12 * SECOND, 0).monotonic).unwrap();
    let now = keeper.snapshot(reading(12 * SECOND, 0).monotonic);
    assert_eq!(now.monotonic, reading(12 * SECOND, 0).monotonic);
    assert_eq!(now.boottime.span_since_origin(), TimeSpan::from_secs(42));
    assert_eq!(now.realtime, SystemTime::from_unix_seconds(1_032));
}

#[def_test]
fn zero_policy_parameters_are_rejected_without_io() {
    for (resolution, max_sleep, max_read) in [(0, 1, 1), (1, 0, 1), (1, 1, 0)] {
        let mut clock = FakeClock::good();
        clock.resolution = TimeSpan::from_secs(resolution);
        assert!(matches!(RtcSleepAccounting::new(clock, TimeSpan::from_secs(max_sleep), TimeSpan::from_secs(max_read)), Err(RtcSleepError::InvalidPolicy)));
    }
}

#[def_test]
fn capture_brackets_device_read_with_monotonic_samples() {
    let mut clock = FakeClock::good();
    let mut calls = 0;
    let sampled = capture(&mut clock, policy(), || { calls += 1; reading(calls * 1_000, calls * 1_000) }).unwrap();
    assert_eq!(sampled.read_span, TimeSpan::from_nanos(1_000));
    assert_eq!(sampled.clocks.monotonic, reading(2_000, 0).monotonic);
    assert_eq!(clock.reads, 1);
}

#[def_test]
fn read_failure_does_not_take_an_after_sample() {
    let mut clock = FakeClock::good();
    clock.value = None;
    let mut calls = 0;
    assert_eq!(capture(&mut clock, policy(), || { calls += 1; reading(0, 0) }).unwrap_err(), RtcSleepError::ReadFailed);
    assert_eq!(clock.reads, 1);
    assert_eq!(calls, 1);
}

#[def_test]
fn slow_or_backwards_sampling_is_rejected() {
    let mut clock = FakeClock::good();
    let mut calls = 0;
    assert_eq!(capture(&mut clock, policy(), || { calls += 1; reading(calls * SECOND, 0) }).unwrap_err(), RtcSleepError::ReadTooSlow);
    let mut calls = 0;
    assert_eq!(capture(&mut clock, policy(), || { calls += 1; reading(SECOND - calls, 0) }).unwrap_err(), RtcSleepError::InvalidMonotonic);
}

#[def_test]
fn changed_source_resolution_is_rejected() {
    let mut clock = FakeClock::good();
    clock.changes_resolution = true;
    assert_eq!(capture(&mut clock, policy(), || reading(0, 0)).unwrap_err(), RtcSleepError::SourceChanged);
    let reads = clock.reads;
    assert_eq!(capture(&mut clock, policy(), || reading(0, 0)).unwrap_err(), RtcSleepError::SourceChanged);
    assert_eq!(clock.reads, reads);
}


#[def_test]
fn explicit_clock_adjustment_discards_phase_reference() {
    let mut owner = RtcSleepAccounting::new(FakeClock::good(), TimeSpan::from_secs(60), TimeSpan::from_millis(10)).unwrap();
    owner.phase = Some(17);
    owner.reset_calibration();
    assert_eq!(owner.phase, None);
    assert_eq!(owner.clock.reads, 0);
}


#[def_test]
fn accounting_requires_a_confirmed_freeze_boundary() {
    let mut boundary = FreezeBoundary::NotReached;
    assert_eq!(boundary.require_reached(), Err(RtcSleepError::NotSuspended));
    assert_eq!(boundary, FreezeBoundary::NotReached);
    assert_eq!(boundary.enter(), Ok(()));
    assert_eq!(boundary.require_reached(), Ok(()));
    assert_eq!(boundary.enter(), Err(RtcSleepError::AlreadySuspended));
    assert_eq!(boundary, FreezeBoundary::Reached);
}
