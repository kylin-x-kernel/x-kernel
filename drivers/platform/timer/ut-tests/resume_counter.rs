// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::ResumeCounter;

fn counter(raw_ticks: u64) -> ResumeCounter {
    let mut state = ResumeCounter::new();
    state.initialize(raw_ticks, raw_ticks);
    state
}

#[def_test]
fn normal_forward_samples_preserve_elapsed_ticks() {
    let mut state = counter(1_000);
    assert_eq!(state.sample(1_025).ticks, 25);
    let sample = state.sample(1_025);
    assert_eq!(sample.ticks, 25);
    assert!(!sample.is_repaired);
    assert_eq!(state.sample(1_050).ticks, 50);
}

#[def_test]
fn reset_preserves_time_and_rebases_subsequent_samples() {
    let mut state = counter(1_000);
    assert_eq!(state.sample(1_100).ticks, 100);
    let sample = state.sample(10);
    assert!(sample.is_repaired);
    assert_eq!(sample.ticks, 100);
    assert_eq!(state.sample(30).ticks, 120);
}

#[def_test]
fn recorded_unsigned_underflow_does_not_publish_huge_time() {
    let mut state = counter(1_389_121_439);
    let sample = state.sample(18_446_744_061_103_844_521);
    assert!(sample.is_repaired);
    assert_eq!(sample.raw_ticks, 18_446_744_061_103_844_521);
    assert_eq!(sample.ticks, 0);
    let next = state.sample(18_446_744_061_153_941_881);
    assert!(!next.is_repaired);
    assert_eq!(next.ticks, 50_097_360);
}

#[def_test]
fn natural_u64_wrap_preserves_small_forward_interval() {
    let mut state = counter(u64::MAX - 10);
    let sample = state.sample(9);
    assert!(!sample.is_repaired);
    assert_eq!(sample.ticks, 20);
}

#[def_test]
fn repeated_suspend_resets_remain_monotonic() {
    let mut state = counter(10_000);
    for round in 1..=3 {
        let sample = state.sample(10);
        assert!(sample.is_repaired);
        assert_eq!(sample.ticks, (round - 1) * 50);
        assert_eq!(state.sample(60).ticks, round * 50);
    }
}

#[def_test]
fn serialized_cpu_samples_do_not_apply_repair_twice() {
    let mut state = counter(10_000);
    assert_eq!(state.sample(10_100).ticks, 100);
    assert!(state.sample(1).is_repaired);
    let other_cpu = state.sample(2);
    assert!(!other_cpu.is_repaired);
    assert_eq!(other_cpu.ticks, 101);
}

#[def_test]
fn half_range_is_rejected_as_ambiguous() {
    let mut state = counter(0);
    let sample = state.sample(1u64 << 63);
    assert!(sample.is_repaired);
    assert_eq!(sample.ticks, 0);
}

#[def_test]
fn physical_origin_preserves_boot_counter() {
    let mut state = ResumeCounter::new();
    state.initialize(1_000, 0);
    assert_eq!(state.sample(1_010).ticks, 1_010);
}
#[def_test]
fn deferred_repair_is_consumed_once_without_resampling() {
    let mut state = counter(1_000);
    assert!(state.take_repair().is_none());
    let sample = state.sample(10);
    state.queue_repair(2, &sample);
    assert_eq!(state.sample(20).ticks, 10);
    let report = state.take_repair().unwrap();
    assert_eq!(report.cpu, 2);
    assert_eq!(report.raw_ticks, 10);
    assert_eq!(report.ticks, 0);
    assert!(state.take_repair().is_none());
}

#[def_test]
fn repeated_repairs_coalesce_and_a_later_repair_remains_pending() {
    let mut state = counter(1_000);
    let sample = state.sample(10);
    state.queue_repair(0, &sample);
    let sample = state.sample(5);
    state.queue_repair(1, &sample);
    assert_eq!(state.take_repair().unwrap().cpu, 0);
    assert!(state.take_repair().is_none());
    let sample = state.sample(1);
    state.queue_repair(3, &sample);
    assert_eq!(state.take_repair().unwrap().cpu, 3);
}

#[def_test]
fn forward_samples_do_not_queue_recovery() {
    let mut state = counter(100);
    let sample = state.sample(101);
    state.queue_repair(0, &sample);
    assert!(state.take_repair().is_none());
}

#[def_test]
fn formal_resume_reanchors_without_repair_or_sleep_progress() {
    let mut counter = ResumeCounter::new();
    counter.initialize(100, 0);
    assert_eq!(counter.sample(150).ticks, 150);
    assert_eq!(counter.resume(0), 150);
    let sample = counter.sample(10);
    assert_eq!(sample.ticks, 160);
    assert!(!sample.is_repaired);
    assert!(counter.take_repair().is_none());
    assert_eq!(counter.resume(1_000_000), 160);
    assert_eq!(counter.sample(1_000_010).ticks, 170);
}


#[def_test]
fn formal_resume_discards_old_deferred_repair_report() {
    let mut counter = ResumeCounter::new();
    counter.initialize(100, 0);
    let repair = counter.sample(0);
    assert!(repair.is_repaired);
    counter.queue_repair(0, &repair);
    assert_eq!(counter.resume(1_000), 100);
    assert!(counter.take_repair().is_none());
    assert!(!counter.sample(1_010).is_repaired);
}
