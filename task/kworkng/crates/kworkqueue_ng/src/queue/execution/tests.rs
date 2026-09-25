// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{
    assert, assert_eq,
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
};

use kcpu_id_map::LogicalCpuId;
use unittest::def_test;

use crate::{queue::tests::queue_for, work::Work};

/// Checks a blocked prefix does not hide later demand or get moved out of order.
///
/// # Panics
/// Panics on premature execution, incorrect demand, ordering or allowance reuse.
#[def_test]
fn demand_skips_blocked_prefix_without_claiming_next() {
    let queue = queue_for(&[0], 3);
    let cpu = LogicalCpuId::new(0);
    let calls = AtomicUsize::new(0);
    let blocked = pin!(Work::with_callback(|| {
        calls.fetch_add(1, Ordering::Relaxed);
    }));
    let first = pin!(Work::with_callback(|| {
        calls.fetch_add(10, Ordering::Relaxed);
    }));
    let next = pin!(Work::with_callback(|| {
        calls.fetch_add(100, Ordering::Relaxed);
    }));
    // SAFETY: all pinned Work and borrowed captures remain live through drain below.
    assert!(unsafe {
        queue
            .submit(&blocked, Some(cpu), || panic!("explicit target"))
            .is_ok()
    });
    let (running, demand) = queue.claim_with_demand(cpu);
    assert!(!demand);
    let running = running.unwrap();
    // SAFETY: retained pinned storage covers both occurrences.
    assert!(unsafe {
        queue
            .submit(&blocked, Some(cpu), || panic!("explicit target"))
            .is_ok()
    });
    // SAFETY: same retained storage/drain obligation for each Work.
    assert!(unsafe {
        queue
            .submit(&first, Some(cpu), || panic!("explicit target"))
            .is_ok()
    });
    // SAFETY: next is canceled below before storage release.
    assert!(unsafe {
        queue
            .submit(&next, Some(cpu), || panic!("explicit target"))
            .is_ok()
    });
    let (selected, demand) = queue.claim_with_demand(cpu);
    assert!(demand);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(
        next.cancel_pending(),
        "demand inspection must not claim next"
    );
    selected.unwrap().run();
    let (none, demand) = queue.claim_with_demand(cpu);
    assert!(none.is_none());
    assert!(!demand, "only a blocked successor remains");
    running.run();
    let (successor, demand) = queue.claim_with_demand(cpu);
    assert!(!demand);
    successor.unwrap().run();
    assert_eq!(calls.load(Ordering::Relaxed), 12);
}

/// Checks an ineligible suffix reports no demand despite free execution allowance.
///
/// # Panics
/// Panics if a running-blocked successor is reported executable or lost.
#[def_test]
fn demand_rejects_blocked_suffix() {
    let queue = queue_for(&[0], 3);
    let cpu = LogicalCpuId::new(0);
    let blocked = pin!(Work::with_callback(|| {}));
    let first = pin!(Work::with_callback(|| {}));
    // SAFETY: pinned Work remains live through both executions.
    assert!(unsafe {
        queue
            .submit(&blocked, Some(cpu), || panic!("explicit target"))
            .is_ok()
    });
    let running = queue.claim_with_demand(cpu).0.unwrap();
    // SAFETY: first is drained before release.
    assert!(unsafe {
        queue
            .submit(&first, Some(cpu), || panic!("explicit target"))
            .is_ok()
    });
    // SAFETY: successor is drained after running below.
    assert!(unsafe {
        queue
            .submit(&blocked, Some(cpu), || panic!("explicit target"))
            .is_ok()
    });
    let (selected, demand) = queue.claim_with_demand(cpu);
    assert!(!demand);
    selected.unwrap().run();
    running.run();
    queue.claim_with_demand(cpu).0.unwrap().run();
}

/// Checks per-CPU routing, capacity and closed-queue draining at the Queue entry.
///
/// # Panics
/// Panics if demand ignores capacity, routes to another CPU, or closure loses work.
#[def_test]
fn demand_respects_cpu_capacity_and_close() {
    let queue = queue_for(&[0, 1], 1);
    let cpu = LogicalCpuId::new(1);
    let first = pin!(Work::with_callback(|| {}));
    let next = pin!(Work::with_callback(|| {}));
    // SAFETY: both pinned objects remain live through final drain.
    assert!(unsafe {
        queue
            .submit(&first, Some(cpu), || panic!("explicit target"))
            .is_ok()
    });
    // SAFETY: same stable storage and drain contract.
    assert!(unsafe {
        queue
            .submit(&next, Some(cpu), || panic!("explicit target"))
            .is_ok()
    });
    for absent in [0, 2] {
        let (execution, demand) = queue.claim_with_demand(LogicalCpuId::new(absent));
        assert!(execution.is_none());
        assert!(!demand);
    }
    queue.close_admission();
    let (execution, demand) = queue.claim_with_demand(cpu);
    assert!(!demand, "claim consumes the only execution allowance");
    let (none, demand) = queue.claim_with_demand(cpu);
    assert!(none.is_none());
    assert!(!demand);
    execution.unwrap().run();
    let (execution, demand) = queue.claim_with_demand(cpu);
    assert!(!demand);
    execution.unwrap().run();
    assert!(queue.claim_with_demand(cpu).0.is_none());
}
