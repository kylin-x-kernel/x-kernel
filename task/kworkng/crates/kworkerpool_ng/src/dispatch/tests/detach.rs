// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Source inspection quiescence, not Work completion or Queue shutdown.

use alloc::{sync::Arc, task::Wake};
use core::{
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Waker},
};

use kpoll::{Completion, PollRegistrations};
use kspin::SpinNoIrq;
use unittest::{assert, assert_eq, def_test};

use crate::{SourceRemainder, dispatch::PartitionDispatch, source::PartitionDispatchState};

fn register(done: &Completion, waker: &Waker) -> PollRegistrations {
    let cx = Context::from_waker(waker);
    let mut registrations = PollRegistrations::new();
    done.register(&mut registrations.context(&cx)).unwrap();
    registrations
}

#[def_test]
fn only_detach_transition_owner_can_publish_completion() {
    for queued in [true, false] {
        let mut state = PartitionDispatchState::new();
        if queued {
            assert!(state.notify());
        }
        assert_eq!(state.detach(), (queued, true));
        // The first owner may still be releasing its removed reference unlocked.
        assert_eq!(state.detach(), (false, false));
        assert!(!state.notify());
    }
}

#[def_test]
fn idle_detach_receipt_is_durable_and_late_notices_do_not_reopen_source() {
    let dispatch = PartitionDispatch::new();
    let source = dispatch.create_source(0usize);
    let done = source.detachment_completion();
    assert!(!done.is_completed());
    dispatch.detach(&source);
    assert!(done.is_completed());
    dispatch.notify(&source);
    dispatch.detach(&source);
    assert!(dispatch.begin_inspection().is_none());
    drop(source);
    drop(dispatch);
    let _late = register(&done, Waker::noop());
    assert!(done.try_wait());
    assert!(done.try_wait());
}

#[def_test]
fn queued_detach_removes_exact_head_middle_or_tail() {
    for removed in 0..3 {
        let dispatch = PartitionDispatch::new();
        let sources = core::array::from_fn::<_, 3, _>(|id| dispatch.create_source(id));
        for source in &sources {
            dispatch.notify(source);
        }
        let done = sources[removed].detachment_completion();
        dispatch.detach(&sources[removed]);
        assert!(done.is_completed());
        dispatch.notify(&sources[removed]);
        for id in (0..3).filter(|id| *id != removed) {
            let turn = dispatch.begin_inspection().unwrap();
            assert_eq!(*turn.source(), id);
            turn.finish(SourceRemainder::Empty);
        }
        assert!(dispatch.begin_inspection().is_none());
    }
}

#[def_test]
fn detach_waits_for_inspection_and_ignores_backlog_notices_and_drop_retry() {
    for disposition in 0..3 {
        let dispatch = PartitionDispatch::new();
        let source = dispatch.create_source(1);
        let done = source.detachment_completion();
        dispatch.notify(&source);
        let turn = dispatch.begin_inspection().unwrap();
        dispatch.notify(&source);
        dispatch.detach(&source);
        dispatch.detach(&source);
        dispatch.notify(&source);
        assert!(!done.is_completed());
        assert!(dispatch.begin_inspection().is_none());
        match disposition {
            0 => turn.finish(SourceRemainder::Empty),
            1 => turn.finish(SourceRemainder::More),
            _ => drop(turn),
        }
        assert!(done.is_completed());
        assert!(dispatch.begin_inspection().is_none());
    }
}

struct InspectWake {
    dispatch: Arc<PartitionDispatch<usize>>,
    calls: AtomicUsize,
}

impl Wake for InspectWake {
    fn wake(self: Arc<Self>) {
        core::assert!(self.dispatch.begin_inspection().is_none());
        self.calls.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn detachment_wakes_after_unlock_exactly_once_in_both_finish_orders() {
    for finish_first in [true, false] {
        let dispatch = PartitionDispatch::new();
        let source = dispatch.create_source(0usize);
        let done = source.detachment_completion();
        let wake = Arc::new(InspectWake {
            dispatch: dispatch.clone(),
            calls: AtomicUsize::new(0),
        });
        let waker = Waker::from(wake.clone());
        let _registered = register(&done, &waker);
        dispatch.notify(&source);
        let turn = dispatch.begin_inspection().unwrap();
        if finish_first {
            turn.finish(SourceRemainder::Empty);
            dispatch.detach(&source);
        } else {
            dispatch.detach(&source);
            assert_eq!(wake.calls.load(Ordering::Relaxed), 0);
            turn.finish(SourceRemainder::Empty);
        }
        dispatch.detach(&source);
        assert!(done.is_completed());
        assert_eq!(wake.calls.load(Ordering::Relaxed), 1);
    }
}

#[def_test]
fn detach_after_state_finish_still_waits_for_reference_retirement() {
    let dispatch = PartitionDispatch::new();
    let source = dispatch.create_source(1);
    let done = source.detachment_completion();
    dispatch.notify(&source);
    let turn = dispatch.begin_inspection().unwrap();
    turn.finish_with_release_hook(SourceRemainder::Empty, || {
        dispatch.detach(&source);
        dispatch.detach(&source);
        core::assert!(!done.is_completed());
    });
    assert!(done.is_completed());
}

#[def_test]
fn overlapping_return_and_next_inspection_have_distinct_cleanup_duties() {
    let dispatch = PartitionDispatch::new();
    let source = dispatch.create_source(1);
    let done = source.detachment_completion();
    dispatch.notify(&source);
    let turn = dispatch.begin_inspection().unwrap();
    turn.finish_with_release_hook(SourceRemainder::More, || {
        let next = dispatch.begin_inspection().unwrap();
        dispatch.detach(&source);
        next.finish(SourceRemainder::Empty);
        core::assert!(!done.is_completed());
    });
    assert!(done.is_completed());
}

struct InspectDrop {
    receipt: Arc<SpinNoIrq<Option<Arc<Completion>>>>,
    drops: Arc<AtomicUsize>,
}

impl Drop for InspectDrop {
    fn drop(&mut self) {
        core::assert!(!self.receipt.lock().as_ref().unwrap().is_completed());
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn final_source_payload_is_destroyed_before_inspection_acknowledgement() {
    let dispatch = PartitionDispatch::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let receipt = Arc::new(SpinNoIrq::new(None));
    let source = dispatch.create_source(InspectDrop {
        receipt: receipt.clone(),
        drops: drops.clone(),
    });
    let done = source.detachment_completion();
    *receipt.lock() = Some(done.clone());
    dispatch.notify(&source);
    let turn = dispatch.begin_inspection().unwrap();
    dispatch.detach(&source);
    drop(source);
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    turn.finish(SourceRemainder::Empty);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
    assert!(done.is_completed());
}
