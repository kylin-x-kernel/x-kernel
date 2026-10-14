// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real owning-list transfers with deterministic unlocked inspection windows.

use alloc::sync::{Arc, Weak};
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use crate::{SourceRemainder, dispatch::PartitionDispatch};

#[path = "dispatch_detach_unittest.rs"]
mod detach;

#[def_test]
fn notifications_coalesce_into_one_owned_list_node() {
    let dispatch = PartitionDispatch::new();
    let source = dispatch.create_source(7);
    for _ in 0..16 {
        dispatch.notify(&source);
    }
    let turn = dispatch.begin_inspection().unwrap();
    assert_eq!(*turn.source(), 7);
    assert!(dispatch.begin_inspection().is_none());
    turn.finish(SourceRemainder::Empty);
    assert!(dispatch.begin_inspection().is_none());
}

#[def_test]
fn empty_inspection_and_notification_retain_exactly_one_next_turn() {
    for notify_first in [true, false] {
        let dispatch = PartitionDispatch::new();
        let source = dispatch.create_source(7);
        dispatch.notify(&source);
        let turn = dispatch.begin_inspection().unwrap();
        if notify_first {
            for _ in 0..8 {
                dispatch.notify(&source);
            }
            turn.finish(SourceRemainder::Empty);
        } else {
            turn.finish(SourceRemainder::Empty);
            for _ in 0..8 {
                dispatch.notify(&source);
            }
        }
        dispatch
            .begin_inspection()
            .unwrap()
            .finish(SourceRemainder::Empty);
        assert!(dispatch.begin_inspection().is_none());
    }
}

#[def_test]
fn unfinished_inspection_retries_without_duplicate_membership() {
    for notified in [true, false] {
        let dispatch = PartitionDispatch::new();
        let source = dispatch.create_source(1);
        dispatch.notify(&source);
        let turn = dispatch.begin_inspection().unwrap();
        if notified {
            dispatch.notify(&source);
        }
        drop(turn);
        dispatch
            .begin_inspection()
            .unwrap()
            .finish(SourceRemainder::Empty);
        assert!(dispatch.begin_inspection().is_none());
    }
}

#[def_test]
fn busy_source_returns_to_tail_and_empty_source_does_not_hide_next() {
    let dispatch = PartitionDispatch::new();
    let first = dispatch.create_source(1);
    let second = dispatch.create_source(2);
    dispatch.notify(&first);
    dispatch.notify(&second);
    let turn = dispatch.begin_inspection().unwrap();
    assert_eq!(*turn.source(), 1);
    turn.finish(SourceRemainder::More);
    let turn = dispatch.begin_inspection().unwrap();
    assert_eq!(*turn.source(), 2);
    turn.finish(SourceRemainder::Empty);
    let turn = dispatch.begin_inspection().unwrap();
    assert_eq!(*turn.source(), 1);
    turn.finish(SourceRemainder::Empty);
    assert!(dispatch.begin_inspection().is_none());
}

#[def_test]
fn independent_sources_can_be_inspected_without_selecting_one_twice() {
    let dispatch = PartitionDispatch::new();
    let first = dispatch.create_source(1);
    let second = dispatch.create_source(2);
    dispatch.notify(&first);
    dispatch.notify(&second);
    let one = dispatch.begin_inspection().unwrap();
    let two = dispatch.begin_inspection().unwrap();
    dispatch.notify(&first);
    assert!(dispatch.begin_inspection().is_none());
    assert_eq!(*one.source(), 1);
    assert_eq!(*two.source(), 2);
    two.finish(SourceRemainder::Empty);
    one.finish(SourceRemainder::Empty);
    dispatch
        .begin_inspection()
        .unwrap()
        .finish(SourceRemainder::Empty);
    assert!(dispatch.begin_inspection().is_none());
}

struct DropProbe {
    dispatch: Weak<PartitionDispatch<DropProbe>>,
    drops: Arc<AtomicUsize>,
}

impl Drop for DropProbe {
    fn drop(&mut self) {
        if let Some(dispatch) = self.dispatch.upgrade() {
            // Reenter the same lock: destruction must not occur under it.
            core::assert!(dispatch.begin_inspection().is_none());
        }
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test]
fn queued_and_selected_turns_keep_payload_until_unlocked_release() {
    let dispatch = PartitionDispatch::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let source = dispatch.create_source(DropProbe {
        dispatch: Arc::downgrade(&dispatch),
        drops: drops.clone(),
    });
    let weak = Arc::downgrade(&source);
    dispatch.notify(&source);
    drop(source);
    assert!(weak.upgrade().is_some());
    let turn = dispatch.begin_inspection().unwrap();
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    turn.finish(SourceRemainder::More);
    assert!(weak.upgrade().is_some());
    dispatch
        .begin_inspection()
        .unwrap()
        .finish(SourceRemainder::Empty);
    assert!(weak.upgrade().is_none());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn idle_association_has_no_dispatcher_owned_payload() {
    let dispatch = PartitionDispatch::new();
    let source = dispatch.create_source(1);
    let weak = Arc::downgrade(&source);
    drop(source);
    assert!(weak.upgrade().is_none());
    assert!(dispatch.begin_inspection().is_none());
}

#[def_test]
fn dropping_private_dispatcher_releases_queued_nodes_without_a_cycle() {
    let dispatch = PartitionDispatch::new();
    let drops = Arc::new(AtomicUsize::new(0));
    let source = dispatch.create_source(DropProbe {
        dispatch: Arc::downgrade(&dispatch),
        drops: drops.clone(),
    });
    let weak_source = Arc::downgrade(&source);
    let weak_dispatch = Arc::downgrade(&dispatch);
    dispatch.notify(&source);
    drop(source);
    drop(dispatch);
    assert!(weak_source.upgrade().is_none());
    assert!(weak_dispatch.upgrade().is_none());
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}
