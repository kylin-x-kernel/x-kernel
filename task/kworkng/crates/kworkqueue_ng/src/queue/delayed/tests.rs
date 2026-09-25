// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Routing integration uses the permanent boot timer service and drains every
//! owned timer. Synthetic execution partitions are never handed to a real worker.

use alloc::sync::Arc;
use core::{cell::Cell, pin::pin};

use ktime_types::TimeSpan;
use unittest::def_test;

use super::{ArmError, CpuPolicy, Queue, SubmitError};
use crate::{queue::config::QueueConfig, work::delayed::DelayedWork};

/// Prepares synthetic execution partitions that tests claim manually.
pub(super) fn queue(policy: CpuPolicy) -> Arc<Queue> {
    Queue::new(QueueConfig::validate(&[0.into(), 2.into(), 4.into()], policy, 1, |_| true).unwrap())
}

/// Returns a representable delay beyond the duration of these tests.
pub(super) fn later() -> TimeSpan {
    TimeSpan::from_nanos(u64::MAX / 2)
}

/// Positive and zero delay share one acceptance-only RR cursor with ordinary work.
///
/// # Panics
/// Panics on rejection/explicit routing consuming a turn, wrong CPU or leaked Queue.
#[def_test]
fn delayed_routing_advances_only_after_fresh_automatic_acceptance() {
    let queue = queue(CpuPolicy::RoundRobin);
    let work = pin!(DelayedWork::new(|| {}));
    let explicit = pin!(DelayedWork::new(|| {}));
    for (index, cpu) in [0, 2, 4, 0, 2, 4].into_iter().enumerate() {
        let delay = if index % 2 == 0 {
            later()
        } else {
            TimeSpan::ZERO
        };
        assert_eq!(
            // SAFETY: both objects stay pinned/shared until each synchronous drain.
            unsafe {
                work.as_ref()
                    .queue_after(&queue, None, delay, || panic!("RR"))
            },
            Ok(cpu.into())
        );
        assert_eq!(
            // SAFETY: retained duplicate must not replace the original occurrence.
            unsafe {
                work.as_ref()
                    .queue_after(&queue, None, later(), || panic!("owner"))
            },
            Err(ArmError::Admission(SubmitError::AlreadyQueued))
        );
        // SAFETY: explicit Work is drained immediately below and service is initialized.
        unsafe { explicit.as_ref().queue_after_on(&queue, 4.into(), later()) }.unwrap();
        assert!(explicit.cancel_sync().unwrap());
        assert_eq!(
            // SAFETY: out-of-range timer programming cannot publish this idle object.
            unsafe {
                explicit
                    .as_ref()
                    .queue_after(&queue, None, TimeSpan::from_nanos(u64::MAX), || {
                        panic!("RR")
                    })
            },
            Err(ArmError::Timer(kerrno::KError::InvalidInput))
        );
        assert!(work.cancel_sync().unwrap());
    }
    let (guard, _) = work.work_for_test().disable();
    assert_eq!(
        // SAFETY: disabled pinned storage outlives this rejected attempt.
        unsafe {
            work.as_ref()
                .queue_after(&queue, None, later(), || panic!("RR"))
        },
        Err(ArmError::Admission(SubmitError::Disabled))
    );
    drop(guard);
    assert_eq!(queue.select_cpu(None, || panic!("RR")), Ok(0.into()));
    queue.close_and_drain().unwrap();
    assert_eq!(
        // SAFETY: closure rejects before publishing on the retained idle Work.
        unsafe {
            work.as_ref()
                .queue_after(&queue, None, later(), || panic!("closed"))
        },
        Err(ArmError::Admission(SubmitError::Closed))
    );
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// A running predecessor fixes queue/CPU ownership without consuming another RR turn.
///
/// # Panics
/// Panics on owner migration, an extra cursor turn or incorrect terminal drain.
#[def_test]
fn delayed_successor_reuses_running_owner() {
    let queue = queue(CpuPolicy::RoundRobin);
    let other = self::queue(CpuPolicy::RoundRobin);
    let work = pin!(DelayedWork::new(|| {}));
    assert_eq!(
        // SAFETY: Work stays pinned through manual predecessor execution and timer drain.
        unsafe {
            work.as_ref()
                .queue_after(&queue, None, TimeSpan::ZERO, || panic!("RR"))
        },
        Ok(0.into())
    );
    let running = queue.partition(0.into()).unwrap().claim_next().unwrap();
    assert_eq!(
        // SAFETY: another queue cannot take the retained running occurrence.
        unsafe {
            work.as_ref()
                .queue_after(&other, None, later(), || panic!("owner"))
        },
        Err(ArmError::Admission(SubmitError::OwnerConflict))
    );
    assert_eq!(
        // SAFETY: explicit migration is rejected before timer programming.
        unsafe { work.as_ref().queue_after_on(&queue, 2.into(), later()) },
        Err(ArmError::Admission(SubmitError::OwnerConflict))
    );
    assert_eq!(
        // SAFETY: same-owner successor remains pinned through the drain below.
        unsafe {
            work.as_ref()
                .queue_after(&queue, None, later(), || panic!("owner"))
        },
        Ok(0.into())
    );
    assert_eq!(queue.select_cpu(None, || panic!("RR")), Ok(2.into()));
    running.run();
    assert!(work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}

/// CurrentCpu selection revalidates an owner appearing before the protected commit.
///
/// # Panics
/// Panics on fallback, stale-owner publication, repeated sampling or leaked Queue.
#[def_test]
fn delayed_current_cpu_selection_retries_changed_owner() {
    let queue = queue(CpuPolicy::CurrentCpu);
    let work = pin!(DelayedWork::new(|| {}));
    assert_eq!(
        // SAFETY: synthetic selection models migration; pinned storage is drained below.
        unsafe {
            work.as_ref()
                .queue_after(&queue, None, later(), || 2.into())
        },
        Ok(2.into())
    );
    assert!(work.cancel_sync().unwrap());
    assert_eq!(
        // SAFETY: invalid CPU is rejected before any physical timer publication.
        unsafe {
            work.as_ref()
                .queue_after(&queue, None, later(), || 3.into())
        },
        Err(ArmError::Admission(SubmitError::InvalidCpu(3.into())))
    );
    let samples = Cell::new(0);
    let sample = || {
        samples.set(samples.get() + 1);
        // SAFETY: inject competing admission on the same pinned Work; final cancel
        // drains it, and the outer attempt must revalidate instead of overwriting it.
        unsafe {
            work.as_ref()
                .queue_after_on(&queue, 0.into(), TimeSpan::ZERO)
        }
        .unwrap();
        2.into()
    };
    assert_eq!(
        // SAFETY: all injected and outer uses share storage retained through final drain.
        unsafe { work.as_ref().queue_after(&queue, None, later(), sample) },
        Err(ArmError::Admission(SubmitError::AlreadyQueued))
    );
    assert_eq!(samples.get(), 1);
    assert!(work.work_for_test().state.lock().pending_is_ready());
    assert_eq!(
        work.work_for_test()
            .state
            .lock()
            .owner_partition()
            .unwrap()
            .cpu_id(),
        0.into()
    );
    assert!(work.cancel_sync().unwrap());
    assert_eq!(Arc::strong_count(&queue), 1);
}
