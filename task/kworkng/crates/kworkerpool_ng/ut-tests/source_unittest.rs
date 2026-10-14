// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Serial orders of service-lock transitions, not a real scheduler or source list.

use core::{assert, assert_eq};

use unittest::def_test;

use super::PartitionDispatchState;
use crate::SourceRemainder;

/// Checks detach owns cleanup once and cannot requeue an outstanding inspection.
///
/// # Panics
/// Panics if detach repeats closure, loses inspection ownership or accepts a notice.
#[def_test]
fn detach_from_every_live_state_is_terminal() {
    for phase in 0..4 {
        for remainder in [SourceRemainder::Empty, SourceRemainder::More] {
            let mut state = PartitionDispatchState::new();
            if phase >= 1 {
                assert!(state.notify());
            }
            if phase >= 2 {
                state.begin_inspection();
            }
            if phase >= 3 {
                assert!(!state.notify());
            }
            assert_eq!(state.detach(), (phase == 1, true));
            assert_eq!(state.detach(), (false, false));
            assert!(!state.notify());
            if phase >= 2 {
                assert_eq!(state, PartitionDispatchState::InspectingDetached);
                assert!(!state.finish_inspection(remainder));
            }
            assert_eq!(state, PartitionDispatchState::Detached);
            assert!(!state.notify());
            assert_eq!(state.detach(), (false, false));
        }
    }
}

/// Checks five-event model including detach for every six-event sequence.
///
/// # Panics
/// Panics if membership, retained notices, inspection or closure diverge from model.
#[def_test]
fn detach_interleavings_match_independent_model() {
    for word in 0..15625usize {
        let mut events = word;
        let mut state = PartitionDispatchState::new();
        let mut has_demand = false;
        let mut is_inspecting = false;
        let mut is_detached = false;
        for _ in 0..6 {
            let event = events % 5;
            events /= 5;
            match event {
                0 => {
                    let insert = !is_detached && !has_demand && !is_inspecting;
                    if !is_detached {
                        has_demand = true;
                    }
                    assert_eq!(state.notify(), insert);
                }
                1 if !is_detached && has_demand && !is_inspecting => {
                    state.begin_inspection();
                    has_demand = false;
                    is_inspecting = true;
                }
                2 | 3 if is_inspecting => {
                    has_demand = !is_detached && (has_demand || event == 3);
                    is_inspecting = false;
                    assert_eq!(
                        state.finish_inspection(if event == 3 {
                            SourceRemainder::More
                        } else {
                            SourceRemainder::Empty
                        }),
                        has_demand
                    );
                }
                4 => {
                    assert_eq!(
                        state.detach(),
                        (!is_detached && has_demand && !is_inspecting, !is_detached)
                    );
                    has_demand = false;
                    is_detached = true;
                }
                _ => break,
            }
            assert_eq!(
                state == PartitionDispatchState::Queued,
                !is_detached && has_demand && !is_inspecting
            );
            assert_eq!(
                state == PartitionDispatchState::Idle,
                !is_detached && !has_demand && !is_inspecting
            );
            assert_eq!(
                state == PartitionDispatchState::InspectingDetached,
                is_detached && is_inspecting
            );
            assert_eq!(
                state == PartitionDispatchState::Detached,
                is_detached && !is_inspecting
            );
        }
    }
}

/// Checks retained source demand against a legal serialized event schedule.
///
/// # Panics
/// Panics if demand is lost, duplicated or retained after it has been consumed.
#[def_test]
fn duplicate_notifications_request_only_one_list_insertion() {
    let mut state = PartitionDispatchState::new();
    assert!(state.notify());
    for _ in 0..16 {
        assert!(!state.notify());
    }
    assert_eq!(state, PartitionDispatchState::Queued);
}

/// Checks retained source demand against a legal serialized event schedule.
///
/// # Panics
/// Panics if demand is lost, duplicated or retained after it has been consumed.
#[def_test]
fn notification_during_empty_inspection_is_not_discarded() {
    let mut state = PartitionDispatchState::new();
    assert!(state.notify());
    state.begin_inspection();
    for _ in 0..16 {
        assert!(!state.notify());
    }
    assert!(state.finish_inspection(SourceRemainder::Empty));
    assert_eq!(state, PartitionDispatchState::Queued);
    // The extra notice buys another inspection, not a permanent busy flag.
    state.begin_inspection();
    assert!(!state.finish_inspection(SourceRemainder::Empty));
    assert_eq!(state, PartitionDispatchState::Idle);
}

/// Checks retained source demand against a legal serialized event schedule.
///
/// # Panics
/// Panics if demand is lost, duplicated or retained after it has been consumed.
#[def_test]
fn either_order_of_notification_and_empty_finish_retains_demand_once() {
    for notify_first in [false, true] {
        let mut state = PartitionDispatchState::new();
        assert!(state.notify());
        state.begin_inspection();
        let mut insertions = 0;
        for is_notification in [notify_first, !notify_first] {
            let needs_insertion = if is_notification {
                state.notify()
            } else {
                state.finish_inspection(SourceRemainder::Empty)
            };
            insertions += usize::from(needs_insertion);
        }
        assert_eq!(insertions, 1);
        assert_eq!(state, PartitionDispatchState::Queued);
    }
}

/// Checks retained source demand against a legal serialized event schedule.
///
/// # Panics
/// Panics if demand is lost, duplicated or retained after it has been consumed.
#[def_test]
fn remaining_backlog_requeues_with_or_without_a_concurrent_notice() {
    for notified in [false, true] {
        let mut state = PartitionDispatchState::new();
        assert!(state.notify());
        state.begin_inspection();
        if notified {
            assert!(!state.notify());
        }
        assert!(state.finish_inspection(SourceRemainder::More));
        assert_eq!(state, PartitionDispatchState::Queued);
        state.begin_inspection();
        assert!(!state.finish_inspection(SourceRemainder::Empty));
    }
}

/// Checks retained source demand against a legal serialized event schedule.
///
/// # Panics
/// Panics if demand is lost, duplicated or retained after it has been consumed.
#[def_test]
fn independent_sources_do_not_share_notification_state() {
    let mut first = PartitionDispatchState::new();
    let mut second = PartitionDispatchState::new();
    assert!(first.notify());
    assert!(second.notify());
    first.begin_inspection();
    assert!(!first.notify());
    second.begin_inspection();
    assert!(!second.finish_inspection(SourceRemainder::Empty));
    assert!(first.finish_inspection(SourceRemainder::Empty));
    assert_eq!(second, PartitionDispatchState::Idle);
}

/// Checks retained source demand against a legal serialized event schedule.
///
/// # Panics
/// Panics if demand is lost, duplicated or retained after it has been consumed.
#[def_test]
fn bounded_legal_event_sequences_match_retained_demand_model() {
    // Explore all six-event words over notify/select/finish-empty/finish-more.
    // The model tracks one pending service obligation, consumed by inspection;
    // it has no knowledge of the implementation's dispatch variants.
    for word in 0..4096usize {
        let mut events = word;
        let mut state = PartitionDispatchState::new();
        let mut has_demand = false;
        let mut is_inspecting = false;
        for _ in 0..6 {
            let event = events % 4;
            events /= 4;
            match event {
                0 => {
                    let should_insert = !has_demand && !is_inspecting;
                    has_demand = true;
                    assert_eq!(state.notify(), should_insert);
                }
                1 if has_demand && !is_inspecting => {
                    state.begin_inspection();
                    has_demand = false;
                    is_inspecting = true;
                }
                2 | 3 if is_inspecting => {
                    let has_more = event == 3;
                    let remainder = if has_more {
                        SourceRemainder::More
                    } else {
                        SourceRemainder::Empty
                    };
                    has_demand |= has_more;
                    is_inspecting = false;
                    assert_eq!(state.finish_inspection(remainder), has_demand);
                }
                _ => break,
            }
            assert_eq!(
                state == PartitionDispatchState::Queued,
                has_demand && !is_inspecting
            );
            assert_eq!(
                state == PartitionDispatchState::Idle,
                !has_demand && !is_inspecting
            );
        }
    }
}
