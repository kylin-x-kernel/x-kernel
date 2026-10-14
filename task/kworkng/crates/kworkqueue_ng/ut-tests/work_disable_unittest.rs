// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::{
    assert, assert_eq,
    num::NonZeroUsize,
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
};

use kcpu_id_map::LogicalCpuId;
use unittest::def_test;

use crate::{queue::partition::QueuePartition, work::Work};

fn partition() -> Arc<QueuePartition> {
    Arc::new(QueuePartition::new(
        LogicalCpuId::new(0),
        NonZeroUsize::new(1).unwrap(),
    ))
}

/// Checks independent guards reopen only after the last release, in either order.
///
/// # Panics
/// Panics if one guard releases another's disable or release automatically submits Work.
#[def_test]
fn independent_disables_release_in_either_order() {
    for reverse in [false, true] {
        let partition = partition();
        let work = pin!(Work::new());
        let (first, revoked) = work.disable();
        assert!(!revoked);
        let (second, revoked) = work.disable();
        assert!(!revoked);
        // SAFETY: pinned Work remains live through all rejected/accepted operations and drain.
        assert!(unsafe { partition.accept(&work, (), 0) }.is_err());
        let remaining = if reverse {
            drop(second);
            first
        } else {
            drop(first);
            second
        };
        // SAFETY: one independent guard still blocks admission to retained pinned storage.
        assert!(unsafe { partition.accept(&work, (), 0) }.is_err());
        drop(remaining);
        assert!(partition.claim_next().is_none());
        // SAFETY: newly accepted Work is drained before its pinned storage is released.
        assert!(unsafe { partition.accept(&work, (), 0) }.is_ok());
        partition.claim_next().unwrap().run();
    }
}

/// Checks idle, waiting, ready, running-only and running-with-successor disable.
///
/// # Panics
/// Panics on wrong revocation result, running cancellation, duplicate callback or restoration.
#[def_test]
fn ordinary_disable_covers_every_phase_without_waiting_for_running() {
    for phase in 0..5 {
        let partition = partition();
        let calls = AtomicUsize::new(0);
        let work = pin!(Work::with_callback(|| {
            calls.fetch_add(1, Ordering::Relaxed);
        }));
        let blocker = pin!(Work::new());
        let blocked = if phase == 1 {
            // SAFETY: blocker is pinned and its execution is consumed below.
            assert!(unsafe { partition.accept(&blocker, (), 0) }.is_ok());
            partition.claim_next()
        } else {
            None
        };
        if phase != 0 {
            // SAFETY: Work is pinned through cancellation and any running execution below.
            assert!(unsafe { partition.accept(&work, (), 0) }.is_ok());
        }
        let running = if phase >= 3 {
            partition.claim_next()
        } else {
            None
        };
        if phase == 4 {
            // SAFETY: successor shares retained pinned storage with its running predecessor.
            assert!(unsafe { partition.accept(&work, (), 1) }.is_ok());
        }
        let (guard, revoked) = work.disable();
        assert_eq!(revoked, matches!(phase, 1 | 2 | 4));
        assert!(partition.claim_next().is_none());
        // SAFETY: all storage remains retained; disable must reject even self-submission.
        assert!(unsafe { partition.accept(&work, (), 0) }.is_err());
        if let Some(running) = running {
            running.run();
        }
        if let Some(blocked) = blocked {
            blocked.run();
        }
        assert_eq!(calls.load(Ordering::Relaxed), usize::from(phase >= 3));
        drop(guard);
        assert!(partition.claim_next().is_none());
        // SAFETY: final new occurrence is drained before releasing pinned captures.
        assert!(unsafe { partition.accept(&work, (), 0) }.is_ok());
        partition.claim_next().unwrap().run();
        assert_eq!(calls.load(Ordering::Relaxed), usize::from(phase >= 3) + 1);
    }
}

struct Resource(Option<Box<dyn FnOnce() + Send>>);

impl Drop for Resource {
    fn drop(&mut self) {
        if let Some(cleanup) = self.0.take() {
            cleanup();
        }
    }
}

/// Checks detached cleanup sees disable held and can take/release an independent guard.
///
/// # Panics
/// Panics if cleanup reenters under locks, admits replacement or releases the outer guard.
#[def_test]
fn disable_remains_held_during_reentrant_cancellation_cleanup() {
    let partition = partition();
    let work = Arc::new(Work::new());
    let weak = Arc::downgrade(&work);
    let cleanup_partition = partition.clone();
    let resource = Resource(Some(Box::new(move || {
        let work = weak.upgrade().unwrap();
        let (nested, revoked) = work.disable();
        assert!(!revoked);
        drop(nested);
        // SAFETY: outer Arc keeps Work stable through this rejected reentrant attempt and drain.
        assert!(unsafe { cleanup_partition.accept(&work, Resource(None), 1) }.is_err());
    })));
    // SAFETY: outer Arc retains stable Work through disable and final acceptance/cancellation.
    assert!(unsafe { partition.accept(&work, resource, 0) }.is_ok());
    let (guard, revoked) = work.disable();
    assert!(revoked);
    assert!(work.state.lock().is_disabled());
    assert!(partition.claim_next().is_none());
    drop(guard);
    // SAFETY: this final occurrence is canceled before releasing the outer Arc.
    assert!(unsafe { partition.accept(&work, Resource(None), 0) }.is_ok());
    assert!(work.cancel_pending());
}
