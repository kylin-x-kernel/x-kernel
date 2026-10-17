// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use unittest::{assert, assert_eq, def_test};

use super::{PidHandle, PidLifecycleState, PidNamespace, root_pid_namespace};

#[def_test]
fn root_pid_handle_starts_at_one() {
    let ns = Arc::new(PidNamespace::new_root());
    let first = PidHandle::allocate_in(&ns).unwrap();
    let second = PidHandle::allocate_in(&ns).unwrap();
    assert_eq!(first.root_nr(), 1);
    assert_eq!(second.root_nr(), 2);
}

#[def_test]
fn child_pid_namespace_gets_pid_one() {
    let root = root_pid_namespace().clone();
    let _root_task = PidHandle::allocate_in(&root).unwrap();
    let child = PidNamespace::new_child(&root);
    let handle = PidHandle::allocate_in(&child).unwrap();

    assert_eq!(handle.nr_in(&child), Some(1));
    assert!(handle.root_nr() >= 2);
    // The active-namespace view is what getpid(2) reports: the deepest
    // projection, not the root number.
    assert_eq!(handle.active_nr(), 1);
    assert!(Arc::ptr_eq(handle.active_ns(), &child));
}

#[def_test]
fn fixed_root_handle_is_visible_to_any_root_namespace() {
    let other_root = Arc::new(PidNamespace::new_root());
    let handle = PidHandle::fixed_root(42);
    assert_eq!(handle.nr_in(&other_root), Some(42));
}

#[def_test]
fn allocated_handle_projects_to_observer_in_ancestor_and_descendant() {
    let root = Arc::new(PidNamespace::new_root());
    let child = PidNamespace::new_child(&root);
    let grandchild = PidNamespace::new_child(&child);
    let handle = PidHandle::allocate_in(&grandchild).unwrap();

    // Observer inside the identity's namespace sees the local number.
    assert_eq!(
        handle.visible_number(&grandchild),
        handle.number_in(&grandchild)
    );
    // An ancestor observer falls back to the nearest ancestor projection.
    assert_eq!(handle.visible_number(&child), handle.number_in(&child));
    assert_eq!(handle.visible_number(&root), Some(handle.root_nr()));
    // A third unrelated root namespace never sees this hierarchy.
    let unrelated_root = Arc::new(PidNamespace::new_root());
    assert_eq!(handle.visible_number(&unrelated_root), None);
}

#[def_test]
fn observer_projection_matches_inside_hierarchy_and_handles_sibling_children() {
    let root = Arc::new(PidNamespace::new_root());
    let left = PidNamespace::new_child(&root);
    let right = PidNamespace::new_child(&root);
    let handle = PidHandle::allocate_in(&left).unwrap();

    assert_eq!(handle.visible_number(&left), handle.number_in(&left));
    // A sibling namespace below the same root is visible at the root level.
    assert_eq!(handle.visible_number(&right), Some(handle.root_nr()));
}

#[def_test]
fn pid_lifecycle_state_machine_rules_hold() {
    // Valid transitions close the intended graph.
    assert_eq!(
        PidLifecycleState::try_transition(
            PidLifecycleState::Reserved,
            PidLifecycleState::Published
        ),
        Some(PidLifecycleState::Published)
    );
    assert_eq!(
        PidLifecycleState::try_transition(
            PidLifecycleState::Reserved,
            PidLifecycleState::Reclaimed
        ),
        Some(PidLifecycleState::Reclaimed)
    );
    assert_eq!(
        PidLifecycleState::try_transition(
            PidLifecycleState::Published,
            PidLifecycleState::Retiring
        ),
        Some(PidLifecycleState::Retiring)
    );
    assert_eq!(
        PidLifecycleState::try_transition(
            PidLifecycleState::Published,
            PidLifecycleState::Reclaimed
        ),
        Some(PidLifecycleState::Reclaimed)
    );
    assert_eq!(
        PidLifecycleState::try_transition(
            PidLifecycleState::Retiring,
            PidLifecycleState::Reclaimed
        ),
        Some(PidLifecycleState::Reclaimed)
    );

    // Every other transition is rejected.
    for (from, to) in [
        (PidLifecycleState::Reserved, PidLifecycleState::Reserved),
        (PidLifecycleState::Published, PidLifecycleState::Reserved),
        (PidLifecycleState::Published, PidLifecycleState::Published),
        (PidLifecycleState::Retiring, PidLifecycleState::Reserved),
        (PidLifecycleState::Retiring, PidLifecycleState::Published),
        (PidLifecycleState::Retiring, PidLifecycleState::Retiring),
        (PidLifecycleState::Reclaimed, PidLifecycleState::Reclaimed),
        (PidLifecycleState::Reclaimed, PidLifecycleState::Reserved),
        (PidLifecycleState::Reclaimed, PidLifecycleState::Published),
        (PidLifecycleState::Reclaimed, PidLifecycleState::Retiring),
    ] {
        assert!(
            PidLifecycleState::try_transition(from, to).is_none(),
            "illegal transition {from:?} -> {to:?} must be rejected"
        );
    }

    // Lookup visibility follows the state machine.
    assert!(!PidLifecycleState::Reserved.is_lookup_visible());
    assert!(PidLifecycleState::Published.is_lookup_visible());
    assert!(PidLifecycleState::Retiring.is_lookup_visible());
    assert!(!PidLifecycleState::Reclaimed.is_lookup_visible());
}

#[def_test]
fn pid_handle_lifecycle_state_machine_is_enforced() {
    let ns = Arc::new(PidNamespace::new_root());
    let handle = PidHandle::allocate_in(&ns).unwrap();

    // Reserved: not lookup-visible; retiring is illegal before publish,
    // but an aborted (never-published) allocation may be reclaimed directly.
    assert_eq!(handle.lifecycle_state(), PidLifecycleState::Reserved);
    assert!(!handle.is_lookup_visible());
    assert!(!handle.mark_retiring());
    assert!(handle.mark_reclaimed());
    assert_eq!(handle.lifecycle_state(), PidLifecycleState::Reclaimed);
    assert!(!handle.is_lookup_visible());

    // A second handle exercises the publish -> retire -> reap path.
    let handle = PidHandle::allocate_in(&ns).unwrap();
    // Reserved: not lookup-visible, cannot retire before publish.
    assert_eq!(handle.lifecycle_state(), PidLifecycleState::Reserved);
    assert!(!handle.is_lookup_visible());
    assert!(!handle.mark_retiring());

    // Publish: transition legal, identity becomes visible.
    assert!(handle.mark_published());
    assert_eq!(handle.lifecycle_state(), PidLifecycleState::Published);
    assert!(handle.is_lookup_visible());
    // Republishing is not a valid transition.
    assert!(!handle.mark_published());

    // Exit path: stays visible while retiring.
    assert!(handle.mark_retiring());
    assert_eq!(handle.lifecycle_state(), PidLifecycleState::Retiring);
    assert!(handle.is_lookup_visible());

    // Reap: identity no longer resolvable and cannot be resurrected.
    assert!(handle.mark_reclaimed());
    assert_eq!(handle.lifecycle_state(), PidLifecycleState::Reclaimed);
    assert!(!handle.is_lookup_visible());
    assert!(!handle.mark_reclaimed());
    assert!(!handle.mark_published());
}

#[def_test]
fn fixed_root_handle_is_published_from_construction() {
    let handle = PidHandle::fixed_root(1);
    assert_eq!(handle.lifecycle_state(), PidLifecycleState::Published);
    assert!(handle.is_lookup_visible());
}
