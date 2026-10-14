// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

fn now() -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(20))
}

/// A monitor borrow safely spans owner release and ends without any GC request.
/// # Panics
/// Panics if identity is lost during inspection or dead weak entries stay after pruning.
#[def_test]
fn owner_release_during_inspection_needs_no_gc() {
    let directory = Directory::new();
    let mut owner = Some(directory.register("queue".into(), 0.into()));
    let weak = Arc::downgrade(owner.as_ref().unwrap());
    owner
        .as_ref()
        .unwrap()
        .publish_interval(Some(MonotonicInstant::ORIGIN));
    let mut reports = 0;
    assert!(directory.inspect(
        0.into(),
        now(),
        TimeSpan::from_secs(10),
        |name, cpu, elapsed| {
            drop(owner.take());
            assert!(weak.upgrade().is_some());
            assert_eq!(name, "queue");
            assert_eq!(cpu, 0.into());
            assert_eq!(elapsed, TimeSpan::from_secs(20));
            reports += 1;
        }
    ));
    assert_eq!(reports, 1);
    assert!(weak.upgrade().is_none());
    assert!(directory.inspect(
        0.into(),
        now(),
        TimeSpan::from_secs(10),
        |_, _, _| unreachable!()
    ));
    assert!(directory.records.lock().is_empty());
}

/// A contended registry is skipped, and an available later pass still warns.
/// # Panics
/// Panics on blocking inspection or a warning lost by a skipped pass.
#[def_test]
fn busy_directory_skips_without_waiting() {
    let directory = Directory::new();
    let record = directory.register("queue".into(), 0.into());
    record.publish_interval(Some(MonotonicInstant::ORIGIN));
    let guard = directory.records.lock();
    assert!(!directory.inspect(
        0.into(),
        now(),
        TimeSpan::from_secs(10),
        |_, _, _| unreachable!()
    ));
    drop(guard);
    let mut reports = 0;
    assert!(
        directory.inspect(0.into(), now(), TimeSpan::from_secs(10), |_, _, _| {
            reports += 1
        })
    );
    assert_eq!(reports, 1);
}

/// Zero suppresses warnings but still prunes records without consuming a warning.
/// # Panics
/// Panics if a zero threshold reports, skips pruning or suppresses a later warning.
#[def_test]
fn zero_threshold_prunes_without_consuming_warning() {
    let directory = Directory::new();
    let live = directory.register("live".into(), 0.into());
    live.publish_interval(Some(MonotonicInstant::ORIGIN));
    let expired = directory.register("expired".into(), 0.into());
    drop(expired);
    assert_eq!(directory.records.lock().len(), 2);

    assert!(
        directory.inspect(0.into(), now(), TimeSpan::ZERO, |_, _, _| {
            panic!("zero threshold must not report")
        })
    );
    assert_eq!(directory.records.lock().len(), 1);

    let mut reports = 0;
    assert!(directory.inspect(
        0.into(),
        now(),
        TimeSpan::from_secs(10),
        |name, cpu, elapsed| {
            assert_eq!(name, "live");
            assert_eq!(cpu, 0.into());
            assert_eq!(elapsed, TimeSpan::from_secs(20));
            reports += 1;
        }
    ));
    assert_eq!(reports, 1);
}

/// CPU filtering preserves other CPUs' warning opportunities.
/// # Panics
/// Panics on wrong identity, cross-CPU suppression or duplicate reporting.
#[def_test]
fn cpu_filter_and_repeat_limit() {
    let directory = Directory::new();
    let a = directory.register("a".into(), 0.into());
    let b = directory.register("b".into(), 1.into());
    a.publish_interval(Some(MonotonicInstant::ORIGIN));
    b.publish_interval(Some(MonotonicInstant::ORIGIN));
    assert!(directory.inspect(
        0.into(),
        now(),
        TimeSpan::from_secs(10),
        |name, _, _| assert_eq!(name, "a")
    ));
    assert!(directory.inspect(
        1.into(),
        now(),
        TimeSpan::from_secs(10),
        |name, _, _| assert_eq!(name, "b")
    ));
    assert!(directory.inspect(
        0.into(),
        now(),
        TimeSpan::from_secs(10),
        |_, _, _| unreachable!()
    ));
}

/// Creation reuses dead registry slots even when no monitor ever runs.
/// # Panics
/// Panics on accumulating dead weak entries or unexpected ownership retention.
#[def_test]
fn creation_prunes_dead_weak_entries() {
    let directory = Directory::new();
    for _ in 0..64 {
        let record = directory.register("queue".into(), 0.into());
        assert_eq!(directory.records.lock().len(), 1);
        let weak = Arc::downgrade(&record);
        drop(record);
        assert!(weak.upgrade().is_none());
    }
}
