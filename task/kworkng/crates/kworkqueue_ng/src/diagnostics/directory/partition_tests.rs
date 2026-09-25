// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real partition transitions with ordinary diagnostic ownership.
//! The boot timer service remains installed; each test-owned timer is drained.

use core::pin::pin;

use ktime_types::TimeSpan;
use unittest::def_test;

use super::*;
use crate::{
    queue::Queue,
    work::{Work, delayed::DelayedWork},
};

fn queue(directory: &Directory) -> Arc<Queue> {
    Queue::with_diagnostics_for_test(&[0.into(), 1.into()], 1, |cpu| {
        directory.register(
            alloc::format!("cpu{}", cpu.as_usize()).into_boxed_str(),
            cpu,
        )
    })
}

/// Production construction binds named records before the first admission.
/// # Panics
/// Panics on missing/wrong warnings, failed cancellation or retained queue storage.
#[def_test]
fn bound_queue_reports_name_and_releases_records() {
    let directory = Directory::new();
    let cpu = khal::percpu::this_cpu_id();
    let config = crate::QueueConfig::new(&[cpu], crate::CpuPolicy::CurrentCpu, 1).unwrap();
    let dispatch = kworkerpool_ng::PartitionDispatch::new();
    let queue = Queue::with_dispatch_and_diagnostics(config, "bound", &directory, |_| {
        (
            dispatch.clone(),
            Arc::new(core::task::Waker::noop().clone()),
        )
    });
    let weak = Arc::downgrade(&queue);
    let work = pin!(Work::with_callback(|| panic!("canceled work ran")));
    // SAFETY: pinned storage remains live until cancellation and queue drain below.
    unsafe { queue.submit(&work, Some(cpu), || unreachable!()) }.unwrap();
    let threshold = TimeSpan::from_secs(20);
    let now = khal::time::monotonic_time() + threshold;
    let mut warnings = 0;
    assert!(
        directory.inspect(cpu, now, threshold, |name, actual_cpu, elapsed| {
            assert_eq!(name, "bound");
            assert_eq!(actual_cpu, cpu);
            assert!(elapsed >= threshold);
            warnings += 1;
        })
    );
    assert_eq!(warnings, 1);
    assert!(work.cancel_pending());
    assert!(
        directory.inspect(cpu, now + threshold, threshold, |_, _, _| {
            panic!("empty queue reported");
        })
    );
    queue.close_and_drain().unwrap();
    drop(queue);
    assert!(weak.upgrade().is_none());
    assert!(
        directory.inspect(cpu, now + threshold, threshold, |_, _, _| {
            panic!("destroyed queue reported");
        })
    );
    assert!(directory.records.lock().is_empty());
}

fn record(directory: &Directory, name: &str) -> Arc<StallRecord> {
    directory
        .records
        .lock()
        .iter()
        .filter_map(Weak::upgrade)
        .find(|record| record.name.as_ref() == name)
        .unwrap()
}

fn snapshot(directory: &Directory, name: &str) -> Option<MonotonicInstant> {
    record(directory, name).interval_start()
}

/// Extra enqueues, rejected admission and intermediate cancellation preserve age.
///
/// # Panics
/// Panics on a false progress report, missing claim progress or a retained backlog.
#[def_test]
fn pending_changes_only_restart_on_execution() {
    let directory = Directory::new();
    let queue = queue(&directory);
    let partition = queue.partition(0.into()).unwrap();
    let a = pin!(Work::with_callback(|| {}));
    let b = pin!(Work::with_callback(|| {}));
    let c = pin!(Work::with_callback(|| {}));
    let initial = snapshot(&directory, "cpu0");
    assert!(initial.is_none());
    // SAFETY: all pinned Work and captures remain live through cancellation/execution below.
    unsafe { queue.submit(&a, Some(0.into()), || unreachable!()) }.unwrap();
    let first = snapshot(&directory, "cpu0");
    assert!(first.is_some());
    // SAFETY: same retained pinned storage; B and C wait for A's reserved allowance.
    unsafe {
        queue.submit(&b, Some(0.into()), || unreachable!()).unwrap();
        queue.submit(&c, Some(0.into()), || unreachable!()).unwrap();
        assert!(queue.submit(&a, Some(0.into()), || unreachable!()).is_err());
    }
    assert_eq!(snapshot(&directory, "cpu0"), first);
    assert!(c.cancel_pending());
    assert_eq!(snapshot(&directory, "cpu0"), first);
    assert!(a.cancel_pending()); // B becomes ready, but has not started.
    assert_eq!(snapshot(&directory, "cpu0"), first);
    // SAFETY: C remains pinned and is canceled after B's execution below.
    unsafe { queue.submit(&c, Some(0.into()), || unreachable!()) }.unwrap();
    assert_eq!(snapshot(&directory, "cpu0"), first);
    // A distinctive synthetic old interval detects a missing claim publication
    // without assuming that two real clock samples have different timestamps.
    let old = MonotonicInstant::from_span_since_origin(TimeSpan::MAX);
    record(&directory, "cpu0").publish_interval(Some(old));
    let execution = partition.claim_next().unwrap();
    let progress = snapshot(&directory, "cpu0");
    assert!(progress.is_some());
    assert_ne!(progress, Some(old));
    execution.run(); // Releasing allowance does not start C yet.
    assert_eq!(snapshot(&directory, "cpu0"), progress);
    assert!(c.cancel_pending());
    assert!(snapshot(&directory, "cpu0").is_none());
    assert_eq!(snapshot(&directory, "cpu1"), initial);
}

/// Running alone is not backlog; its blocked successor is, even before release.
///
/// # Panics
/// Panics if running is reported as backlog or releasing a successor resets age.
#[def_test]
fn running_successor_stays_backlogged_through_release() {
    let directory = Directory::new();
    let queue = queue(&directory);
    let partition = queue.partition(0.into()).unwrap();
    let work = pin!(Work::with_callback(|| {}));
    // SAFETY: storage stays pinned through both occurrences and their cleanup.
    unsafe { queue.submit(&work, Some(0.into()), || unreachable!()) }.unwrap();
    let running = partition.claim_next().unwrap();
    assert!(snapshot(&directory, "cpu0").is_none());
    // SAFETY: the same pinned object remains live through the successor execution.
    unsafe { queue.submit(&work, Some(0.into()), || unreachable!()) }.unwrap();
    let waiting = snapshot(&directory, "cpu0");
    assert!(waiting.is_some());
    assert!(partition.claim_next().is_none());
    assert_eq!(snapshot(&directory, "cpu0"), waiting);
    running.run();
    assert_eq!(snapshot(&directory, "cpu0"), waiting);
    partition.claim_next().unwrap().run();
    assert!(snapshot(&directory, "cpu0").is_none());
}

/// Timer-only placement starts no episode; delivery and withdrawal change backlog.
///
/// # Panics
/// Panics if a future timer is counted, due transfer is missed or withdrawal stays active.
#[def_test]
fn delayed_timer_membership_controls_backlog() {
    let directory = Directory::new();
    let queue = queue(&directory);
    let work = pin!(DelayedWork::new(|| {}));
    let far = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(u64::MAX));
    let initial = snapshot(&directory, "cpu0");
    // SAFETY: pinned delayed storage and its timer remain live through final cancel_sync.
    unsafe { work.as_ref().arm_on(&queue, 0.into(), far) }.unwrap();
    assert_eq!(snapshot(&directory, "cpu0"), initial);
    assert!(
        // SAFETY: the matching timer remains prepared and pinned until the final drain.
        unsafe {
            work.as_ref()
                .retime_pending_at(khal::time::monotonic_time())
        }
        .unwrap()
    );
    // SAFETY: the pinned Work remains live; the observation follows the due
    // deadline. Explicit delivery makes progress independent of device IRQ timing;
    // a physical delivery that already won the partition lock makes this a no-op.
    unsafe {
        queue
            .partition(0.into())
            .unwrap()
            .queue_due(work.work_for_test(), khal::time::monotonic_time_nanos())
    }
    .unwrap();
    let queued = snapshot(&directory, "cpu0");
    assert!(queued.is_some());
    assert!(
        // SAFETY: failed range validation leaves the same retained timer/Work untouched.
        unsafe {
            work.as_ref()
                .retime_pending_at(MonotonicInstant::from_span_since_origin(TimeSpan::MAX))
        }
        .is_err()
    );
    assert_eq!(snapshot(&directory, "cpu0"), queued);
    // SAFETY: the retained matching timer can take over the ordinary pending occurrence.
    assert!(unsafe { work.as_ref().retime_pending_at(far) }.unwrap());
    let withdrawn = snapshot(&directory, "cpu0");
    assert!(withdrawn.is_none());
    // SAFETY: zero-delay modification reuses the same pinned timer/Work pair.
    unsafe {
        work.as_ref()
            .modify_after(&queue, Some(0.into()), TimeSpan::ZERO, || unreachable!())
    }
    .unwrap();
    assert!(snapshot(&directory, "cpu0").is_some());
    // SAFETY: withdraw again before synchronously draining the retained timer.
    assert!(unsafe { work.as_ref().retime_pending_at(far) }.unwrap());
    let withdrawn = snapshot(&directory, "cpu0");
    assert!(withdrawn.is_none());
    assert!(work.cancel_sync().unwrap());
    assert_eq!(snapshot(&directory, "cpu0"), withdrawn);
}

/// Each CPU retains its own episode across another CPU's execution.
///
/// # Panics
/// Panics if one CPU's progress alters another's episode.
#[def_test]
fn other_cpu_progress_preserves_age() {
    let directory = Directory::new();
    let queue = queue(&directory);
    let a = pin!(Work::with_callback(|| {}));
    let b = pin!(Work::with_callback(|| {}));
    // SAFETY: both pinned objects remain live through their manual executions below.
    unsafe {
        queue.submit(&a, Some(0.into()), || unreachable!()).unwrap();
        queue.submit(&b, Some(1.into()), || unreachable!()).unwrap();
    }
    let first = snapshot(&directory, "cpu0");
    queue
        .partition(1.into())
        .unwrap()
        .claim_next()
        .unwrap()
        .run();
    assert_eq!(snapshot(&directory, "cpu0"), first);
    assert!(snapshot(&directory, "cpu1").is_none());
    queue
        .partition(0.into())
        .unwrap()
        .claim_next()
        .unwrap()
        .run();
    queue.close_and_drain().unwrap();
    assert!(snapshot(&directory, "cpu0").is_none());
}

/// Retaining diagnostic data never extends the semantic queue's lifetime.
/// # Panics
/// Panics on a Queue ownership cycle or premature diagnostic release.
#[def_test]
fn diagnostic_reference_does_not_retain_queue() {
    let directory = Directory::new();
    let queue = queue(&directory);
    let queue_weak = Arc::downgrade(&queue);
    let observation = record(&directory, "cpu0");
    let record_weak = Arc::downgrade(&observation);
    drop(queue);
    assert!(queue_weak.upgrade().is_none());
    assert!(observation.interval_start().is_none());
    drop(observation);
    assert!(record_weak.upgrade().is_none());
}
