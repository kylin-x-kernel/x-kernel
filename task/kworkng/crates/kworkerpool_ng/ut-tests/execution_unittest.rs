// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Contract fixtures, not concrete WorkQueue claims or real BH/worker contexts.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use core::{
    num::NonZeroUsize,
    sync::atomic::{AtomicUsize, Ordering},
};

use kspin::SpinNoIrq;
use unittest::{assert, assert_eq, def_test};

use crate::{
    SourceRemainder,
    dispatch::{PartitionDispatch, PartitionSource},
    execution::{AcquiredExecution, PartitionWorkSource},
};

struct TestSource(Box<dyn Fn() -> (Option<TestExecution>, SourceRemainder) + Send + Sync>);

struct TestExecution(Box<dyn FnOnce() + Send>);

impl PartitionWorkSource for TestSource {
    type Execution = TestExecution;

    fn acquire(&self) -> (Option<Self::Execution>, SourceRemainder) {
        self.0()
    }
}

impl AcquiredExecution for TestExecution {
    fn run(self) {
        self.0();
    }
}

#[def_test]
fn empty_source_counts_as_a_turn_and_does_not_hide_other_sources() {
    let dispatch = PartitionDispatch::new();
    assert!(!dispatch.run_one_turn());
    let runs = Arc::new(AtomicUsize::new(0));
    let empty = dispatch.create_source(TestSource(Box::new(|| (None, SourceRemainder::Empty))));
    let counter = runs.clone();
    let ready = dispatch.create_source(TestSource(Box::new(move || {
        let counter = counter.clone();
        (
            Some(TestExecution(Box::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }))),
            SourceRemainder::Empty,
        )
    })));
    dispatch.notify(&empty);
    dispatch.notify(&ready);
    assert!(dispatch.run_one_turn());
    assert_eq!(runs.load(Ordering::Relaxed), 0);
    assert!(dispatch.run_one_turn());
    assert_eq!(runs.load(Ordering::Relaxed), 1);
    assert!(!dispatch.run_one_turn());
}

#[def_test]
fn source_turn_is_finished_before_execution_enters_callback() {
    let dispatch = PartitionDispatch::new();
    let weak = Arc::downgrade(&dispatch);
    let runs = Arc::new(AtomicUsize::new(0));
    let counter = runs.clone();
    let source = dispatch.create_source(TestSource(Box::new(move || {
        // Reentry also proves acquire does not hold the service lock.
        core::assert!(weak.upgrade().unwrap().begin_inspection().is_none());
        let weak = weak.clone();
        let counter = counter.clone();
        (
            Some(TestExecution(Box::new(move || {
                // This source must already be available for the next inspection.
                let dispatch = weak.upgrade().unwrap();
                dispatch
                    .begin_inspection()
                    .unwrap()
                    .finish(SourceRemainder::Empty);
                counter.fetch_add(1, Ordering::Relaxed);
            }))),
            SourceRemainder::More,
        )
    })));
    dispatch.notify(&source);
    assert!(dispatch.run_one_turn());
    assert_eq!(runs.load(Ordering::Relaxed), 1);
    assert!(!dispatch.run_one_turn());
}

#[def_test]
fn notification_inside_empty_acquire_survives_turn_completion() {
    let dispatch = PartitionDispatch::new();
    let weak = Arc::downgrade(&dispatch);
    let locator = Arc::new(SpinNoIrq::new(Weak::<PartitionSource<TestSource>>::new()));
    let saved = locator.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let source = dispatch.create_source(TestSource(Box::new(move || {
        if counter.fetch_add(1, Ordering::Relaxed) == 0 {
            let source = saved.lock().upgrade().unwrap();
            weak.upgrade().unwrap().notify(&source);
        }
        (None, SourceRemainder::Empty)
    })));
    *locator.lock() = Arc::downgrade(&source);
    dispatch.notify(&source);
    assert!(dispatch.run_one_turn());
    assert!(dispatch.run_one_turn());
    assert!(!dispatch.run_one_turn());
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[def_test]
fn retained_empty_demand_does_not_make_a_single_turn_loop() {
    let dispatch = PartitionDispatch::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let source = dispatch.create_source(TestSource(Box::new(move || {
        counter.fetch_add(1, Ordering::Relaxed);
        (None, SourceRemainder::More)
    })));
    dispatch.notify(&source);
    for expected in 1..=4 {
        assert!(dispatch.run_one_turn());
        assert_eq!(calls.load(Ordering::Relaxed), expected);
    }
    dispatch
        .begin_inspection()
        .unwrap()
        .finish(SourceRemainder::Empty);
    assert!(!dispatch.run_one_turn());
}

#[def_test]
fn bounded_pass_stops_on_empty_list_and_counts_empty_demand() {
    let dispatch = PartitionDispatch::new();
    let budget = NonZeroUsize::new(3).unwrap();
    assert!(!dispatch.run_bounded_pass(budget));
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    let source = dispatch.create_source(TestSource(Box::new(move || {
        counter.fetch_add(1, Ordering::Relaxed);
        (None, SourceRemainder::More)
    })));
    dispatch.notify(&source);
    assert!(dispatch.run_bounded_pass(budget));
    assert_eq!(calls.load(Ordering::Relaxed), 3);
    dispatch.detach(&source);
    assert!(!dispatch.run_bounded_pass(budget));
    assert_eq!(calls.load(Ordering::Relaxed), 3);
}

#[def_test]
fn bounded_pass_preserves_rotation_across_budget_exits() {
    let dispatch = PartitionDispatch::new();
    let trace = Arc::new(AtomicUsize::new(0));
    for id in 1..=2 {
        let trace = trace.clone();
        let source = dispatch.create_source(TestSource(Box::new(move || {
            let trace = trace.clone();
            (
                Some(TestExecution(Box::new(move || {
                    let previous = trace.load(Ordering::Relaxed);
                    trace.store(previous * 10 + id, Ordering::Relaxed);
                }))),
                SourceRemainder::More,
            )
        })));
        dispatch.notify(&source);
    }
    assert!(dispatch.run_bounded_pass(NonZeroUsize::new(3).unwrap()));
    assert_eq!(trace.load(Ordering::Relaxed), 121);
    assert!(dispatch.run_bounded_pass(NonZeroUsize::new(1).unwrap()));
    assert_eq!(trace.load(Ordering::Relaxed), 1212);
}

#[def_test]
fn empty_turn_exhausts_budget_without_consuming_next_callback() {
    let dispatch = PartitionDispatch::new();
    let calls = Arc::new(AtomicUsize::new(0));
    let empty = dispatch.create_source(TestSource(Box::new(|| (None, SourceRemainder::Empty))));
    let counter = calls.clone();
    let ready = dispatch.create_source(TestSource(Box::new(move || {
        let counter = counter.clone();
        (
            Some(TestExecution(Box::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }))),
            SourceRemainder::Empty,
        )
    })));
    dispatch.notify(&empty);
    dispatch.notify(&ready);
    let budget = NonZeroUsize::new(1).unwrap();
    assert!(dispatch.run_bounded_pass(budget));
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert!(!dispatch.run_bounded_pass(budget));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[def_test]
fn final_callback_notification_is_retained_for_next_pass() {
    let dispatch = PartitionDispatch::new();
    let weak = Arc::downgrade(&dispatch);
    let next = dispatch.create_source(TestSource(Box::new(|| (None, SourceRemainder::Empty))));
    let next_source = next.clone();
    let source = dispatch.create_source(TestSource(Box::new(move || {
        let weak = weak.clone();
        let next = next_source.clone();
        (
            Some(TestExecution(Box::new(move || {
                weak.upgrade().unwrap().notify(&next);
            }))),
            SourceRemainder::Empty,
        )
    })));
    dispatch.notify(&source);
    let budget = NonZeroUsize::new(1).unwrap();
    assert!(dispatch.run_bounded_pass(budget));
    assert!(!dispatch.run_bounded_pass(budget));
    // A notification after the empty observation needs a fresh provider wake.
    dispatch.notify(&next);
    assert!(dispatch.run_one_turn());
    assert!(!dispatch.run_one_turn());
}

struct DropCount(Arc<AtomicUsize>);

impl Drop for DropCount {
    fn drop(&mut self) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn owned_execution_outlives_last_source_reference_and_cleans_up_once() {
    let dispatch = PartitionDispatch::new();
    let source_drops = Arc::new(AtomicUsize::new(0));
    let execution_drops = Arc::new(AtomicUsize::new(0));
    let runs = Arc::new(AtomicUsize::new(0));
    let source_lifetime = DropCount(source_drops.clone());
    let cleanup = execution_drops.clone();
    let counter = runs.clone();
    let source = dispatch.create_source(TestSource(Box::new(move || {
        let source_drops = source_lifetime.0.clone();
        let execution_lifetime = DropCount(cleanup.clone());
        let counter = counter.clone();
        (
            Some(TestExecution(Box::new(move || {
                core::assert_eq!(source_drops.load(Ordering::Relaxed), 1);
                counter.fetch_add(1, Ordering::Relaxed);
                drop(execution_lifetime);
            }))),
            SourceRemainder::Empty,
        )
    })));
    dispatch.notify(&source);
    drop(source);
    assert!(dispatch.run_one_turn());
    assert!(!dispatch.run_one_turn());
    assert_eq!(source_drops.load(Ordering::Relaxed), 1);
    assert_eq!(execution_drops.load(Ordering::Relaxed), 1);
    assert_eq!(runs.load(Ordering::Relaxed), 1);
}

struct BorrowedSource<'a> {
    runs: &'a AtomicUsize,
    source_drops: &'a AtomicUsize,
}

struct BorrowedExecution<'a> {
    runs: &'a AtomicUsize,
    source_drops: &'a AtomicUsize,
}

impl Drop for BorrowedSource<'_> {
    fn drop(&mut self) {
        self.source_drops.fetch_add(1, Ordering::Relaxed);
    }
}

impl<'a> PartitionWorkSource for BorrowedSource<'a> {
    type Execution = BorrowedExecution<'a>;

    fn acquire(&self) -> (Option<Self::Execution>, SourceRemainder) {
        (
            Some(BorrowedExecution {
                runs: self.runs,
                source_drops: self.source_drops,
            }),
            SourceRemainder::Empty,
        )
    }
}

impl AcquiredExecution for BorrowedExecution<'_> {
    fn run(self) {
        core::assert_eq!(self.source_drops.load(Ordering::Relaxed), 1);
        self.runs.fetch_add(1, Ordering::Relaxed);
    }
}

/// A claim may borrow external storage without retaining its source or allocating a wrapper.
#[def_test]
fn borrowed_execution_survives_source_retirement_while_external_owner_lives() {
    let runs = AtomicUsize::new(0);
    let source_drops = AtomicUsize::new(0);
    let dispatch = PartitionDispatch::new();
    let source = dispatch.create_source(BorrowedSource {
        runs: &runs,
        source_drops: &source_drops,
    });
    dispatch.notify(&source);
    drop(source);
    assert!(dispatch.run_one_turn());
    assert_eq!(runs.load(Ordering::Relaxed), 1);
    assert_eq!(source_drops.load(Ordering::Relaxed), 1);
    assert!(!dispatch.run_one_turn());
}
