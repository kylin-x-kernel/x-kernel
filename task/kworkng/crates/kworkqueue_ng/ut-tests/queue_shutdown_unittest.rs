// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::{
    assert, assert_eq,
    sync::atomic::{AtomicBool, Ordering},
};

use kcpu_id_map::LogicalCpuId;
use unittest::def_test;

use super::*;
use crate::{
    queue::{
        config::{CpuPolicy, QueueConfig},
        submission::SubmitError,
    },
    work::Work,
};

fn queue() -> Arc<Queue> {
    Queue::new(QueueConfig::new(&[LogicalCpuId::new(0)], CpuPolicy::CurrentCpu, 1).unwrap())
}

/// Even idle/repeated calls validate context, and rejection leaves admission open.
#[def_test]
fn close_drain_preflight_and_repeated_calls() {
    let queue = queue();
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        assert_eq!(queue.close_and_drain(), Err(WaitError::InvalidContext));
    }
    assert!(queue.admission_color().is_some());
    assert_eq!(queue.close_and_drain(), Ok(()));
    assert_eq!(queue.close_and_drain(), Ok(()));
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        assert_eq!(queue.close_and_drain(), Err(WaitError::InvalidContext));
    }
    let work = core::pin::pin!(Work::with_callback(|| {}));
    assert_eq!(
        // SAFETY: the pinned Work remains live; closed admission must publish nothing.
        unsafe { queue.submit(&work, None, || 0.into()) },
        Err(SubmitError::Closed)
    );
}

/// Closure rejects successors, but a queued predecessor remains executable for drain.
#[def_test]
fn concurrent_queue_drains_wait_for_accepted_work() {
    let queue = queue();
    let work = Arc::new(Work::with_callback(|| {}));
    // SAFETY: this Arc remains live until all execution/drain calls are joined.
    assert!(unsafe { queue.submit(&work, None, || 0.into()) }.is_ok());
    let drains = core::array::from_fn::<_, 2, _>(|_| {
        let queue = queue.clone();
        ktask::spawn(move || assert_eq!(queue.close_and_drain(), Ok(())))
    });
    while drains
        .iter()
        .any(|task| task.state() != ktask::TaskState::Blocked)
    {
        ktask::yield_now();
    }
    assert!(queue.admission_color().is_none());
    queue.claim_with_demand(0.into()).0.unwrap().run();
    for task in drains {
        assert_eq!(task.join(), 0);
    }
    assert_eq!(work.observer_count(), 0);
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// Nested callbacks retain the outer queue identity and permit unrelated drains.
///
/// # Panics
/// Panics on lost outer identity, premature closure or stale callback membership.
#[cfg(feature = "self-wait-check")]
#[def_test]
fn nested_callbacks_keep_original_queue_identity() {
    let outer = queue();
    let inner = queue();
    let unrelated = queue();
    let nested = core::pin::pin!(Work::with_callback(|| {
        assert_eq!(outer.close_and_drain(), Err(WaitError::SelfWait));
        assert_eq!(inner.close_and_drain(), Err(WaitError::SelfWait));
        assert!(outer.admission_color().is_some());
        assert!(inner.admission_color().is_some());
        assert_eq!(unrelated.close_and_drain(), Ok(()));
    }));
    let work = core::pin::pin!(Work::with_callback(|| {
        // SAFETY: nested is pinned and its callback completes before this returns.
        unsafe { inner.submit(&nested, None, || 0.into()) }.unwrap();
        inner.claim_with_demand(0.into()).0.unwrap().run();
        assert_eq!(outer.close_and_drain(), Err(WaitError::SelfWait));
    }));
    for _ in 0..2 {
        // SAFETY: both pinned objects/captures survive execution and final drains.
        unsafe { outer.submit(&work, None, || 0.into()) }.unwrap();
        outer.claim_with_demand(0.into()).0.unwrap().run();
    }
    assert_eq!(outer.close_and_drain(), Ok(()));
    assert_eq!(inner.close_and_drain(), Ok(()));
}

/// Completion on one partition does not hide another partition's unfinished work.
#[def_test]
fn queue_drain_covers_every_configured_partition() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let queue =
        Queue::new(QueueConfig::new(&[0.into(), 1.into()], CpuPolicy::CurrentCpu, 1).unwrap());
    let first = core::pin::pin!(Work::with_callback(|| {}));
    let second = core::pin::pin!(Work::with_callback(|| {}));
    // SAFETY: pinned Work storage survives both executions and the joined drain.
    assert!(unsafe { queue.submit(&first, Some(0.into()), || unreachable!()) }.is_ok());
    // SAFETY: pinned Work storage survives both executions and the joined drain.
    assert!(unsafe { queue.submit(&second, Some(1.into()), || unreachable!()) }.is_ok());
    let returned = Arc::new(AtomicBool::new(false));
    let drain = ktask::spawn({
        let (queue, returned) = (queue.clone(), returned.clone());
        move || {
            assert_eq!(queue.close_and_drain(), Ok(()));
            returned.store(true, Ordering::Release);
        }
    });
    while drain.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    // This controlled executor intentionally tests accounting, not CPU affinity.
    queue.claim_with_demand(0.into()).0.unwrap().run();
    assert!(!returned.load(Ordering::Acquire));
    queue.claim_with_demand(1.into()).0.unwrap().run();
    assert_eq!(drain.join(), 0);
    assert!(returned.load(Ordering::Acquire));
}

struct Resource(Option<Box<dyn FnOnce() + Send>>);
impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(cleanup) = self.0.take() {
            cleanup();
        }
    }
}

/// Queue drain includes already detached cancellation cleanup in every color.
#[def_test]
fn queue_drain_covers_detached_cleanup_in_all_colors() {
    for color in 0..COLOR_COUNT {
        let queue = queue();
        let work = Arc::new(Work::new());
        let cleanup_started = Arc::new(AtomicBool::new(false));
        let release_cleanup = Arc::new(AtomicBool::new(false));
        let resource = Resource(Some(Box::new({
            let (started, release) = (cleanup_started.clone(), release_cleanup.clone());
            move || {
                started.store(true, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
            }
        })));
        // SAFETY: this retained Arc is stable until cancellation and queue drain join.
        assert!(unsafe { queue.partitions[0].accept(&work, resource, color) }.is_ok());
        let cancel = ktask::spawn({
            let work = work.clone();
            move || assert!(work.cancel_pending())
        });
        while !cleanup_started.load(Ordering::Acquire) {
            ktask::yield_now();
        }
        assert_eq!(work.observer_count(), 0);
        let drain = ktask::spawn({
            let queue = queue.clone();
            move || assert_eq!(queue.close_and_drain(), Ok(()))
        });
        while drain.state() != ktask::TaskState::Blocked {
            ktask::yield_now();
        }
        release_cleanup.store(true, Ordering::Release);
        assert_eq!(cancel.join(), 0);
        assert_eq!(drain.join(), 0);
    }
}
