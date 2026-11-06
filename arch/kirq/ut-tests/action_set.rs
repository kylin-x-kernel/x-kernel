// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec::Vec};

use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{IrqEvent, platform::Handler};

fn handler() -> Handler {
    Arc::new(|_| IrqEvent::HANDLED)
}

#[def_test]
fn action_chain_transfer_trait_dependencies() {
    fn requires_send_sync<T: Send + Sync>() {}
    fn requires_send<T: Send>() {}

    requires_send_sync::<Handler>();
    requires_send_sync::<IrqAction>();
    requires_send_sync::<ActionRecord>();
    requires_send_sync::<KBox<ActionRecord>>();
    requires_send::<PreparedAction>();
    requires_send::<ActionSet>();
    requires_send::<RetiredAction>();
}

/// Private fixture removal after every scoped traversal has ended.
fn remove(actions: &mut ActionSet, token: Option<IrqActionToken>) -> Option<IrqAction> {
    let retired = actions.withdraw(token)?;
    // SAFETY: callers own this private set and have consumed all snapshots;
    // there are no concurrent readers, pending admissions or short locks.
    Some(unsafe { retired.reclaim() })
}

#[def_test]
fn ordinary_registration_kinds_cannot_mix_and_rejection_retains_preparation() {
    let mut actions = ActionSet::new();
    let mut regular = PreparedAction::regular(handler()).unwrap();
    let mut shared = PreparedAction::shared(handler()).unwrap();
    assert!(actions.publish_regular(&mut regular));
    assert!(regular.pending.is_none());
    assert!(!actions.can_install_shared());
    assert!(actions.publish_shared(&mut shared).is_none());
    assert!(shared.pending.is_some());
    assert!(remove(&mut actions, Some(IrqActionToken::new(0))).is_none());
    assert_eq!(actions.len(), 1);
    assert!(remove(&mut actions, None).is_some());
    let token = actions.publish_shared(&mut shared).unwrap();
    let mut regular = PreparedAction::regular(handler()).unwrap();
    assert!(!actions.publish_regular(&mut regular));
    assert!(regular.pending.is_some());
    assert!(remove(&mut actions, None).is_none());
    assert_eq!(actions.len(), 1);
    assert!(remove(&mut actions, Some(token)).is_some());
    assert_eq!(actions.len(), 0);
}

#[def_test]
fn shared_membership_exceeds_four_and_wrong_removal_preserves_count() {
    let mut actions = ActionSet::new();
    let mut tokens = Vec::new();
    for _ in 0..8 {
        let mut prepared = PreparedAction::shared(handler()).unwrap();
        tokens.push(actions.publish_shared(&mut prepared).unwrap());
    }
    assert_eq!(actions.len(), 8);
    assert!(remove(&mut actions, Some(tokens[3])).is_some());
    assert!(remove(&mut actions, Some(tokens[3])).is_none());
    assert!(remove(&mut actions, Some(IrqActionToken::new(usize::MAX))).is_none());
    assert!(remove(&mut actions, None).is_none());
    assert_eq!(actions.len(), 7);
    let mut seen = [0; 7];
    let mut count = 0;
    // SAFETY: this private set remains alive and unmodified until consumption.
    unsafe { actions.snapshot() }.with_actions(|action| {
        seen[count] = action.token().id();
        count += 1;
    });
    assert_eq!(count, 7);
    assert_eq!(seen, [1, 2, 3, 5, 6, 7, 8]);
    for (index, token) in tokens.into_iter().enumerate() {
        if index != 3 {
            assert!(remove(&mut actions, Some(token)).is_some());
        }
    }
    assert_eq!(actions.len(), 0);
}

#[def_test]
fn scoped_snapshot_borrows_without_cloning_handler_owners() {
    let owner = handler();
    let mut actions = ActionSet::new();
    let mut prepared = PreparedAction::shared(owner.clone()).unwrap();
    let token = actions.publish_shared(&mut prepared).unwrap();
    // SAFETY: all nodes and the private set outlive this consuming traversal.
    let snapshot = unsafe { actions.snapshot() };
    assert_eq!(Arc::strong_count(&owner), 2);
    let mut observed = false;
    snapshot.with_actions(|action| {
        observed = action.token() == token && action.run_primary(0).handled();
    });
    assert!(observed);
    assert_eq!(Arc::strong_count(&owner), 2);
    let removed = remove(&mut actions, Some(token)).unwrap();
    assert_eq!(Arc::strong_count(&owner), 2);
    drop(removed);
    assert_eq!(Arc::strong_count(&owner), 1);
}

#[def_test]
fn linked_owner_drops_only_after_scoped_snapshot_finishes() {
    let owner = handler();
    let mut actions = ActionSet::new();
    let mut prepared = PreparedAction::regular(owner.clone()).unwrap();
    assert!(actions.publish_regular(&mut prepared));
    // SAFETY: the private set is dropped only after this snapshot is consumed.
    let snapshot = unsafe { actions.snapshot() };
    let mut handled = false;
    snapshot.with_actions(|action| handled = action.run_primary(0).handled());
    assert!(handled);
    assert_eq!(Arc::strong_count(&owner), 2);
    drop(actions);
    assert_eq!(Arc::strong_count(&owner), 1);
}

#[def_test]
fn snapshot_ceiling_excludes_append_reached_through_existing_successor() {
    let mut actions = ActionSet::new();
    let mut first = PreparedAction::shared(handler()).unwrap();
    let first_token = actions.publish_shared(&mut first).unwrap();
    let mut second = PreparedAction::shared(handler()).unwrap();
    let second_token = actions.publish_shared(&mut second).unwrap();
    // SAFETY: appending cannot invalidate any node; no reclamation or set
    // destruction occurs until this private traversal has been consumed.
    let snapshot = unsafe { actions.snapshot() };
    let mut appended = PreparedAction::shared(handler()).unwrap();
    let mut appended_token = None;
    let mut seen = [0; 3];
    let mut calls = 0;
    snapshot.with_actions(|action| {
        seen[calls] = action.token().id();
        calls += 1;
        if calls == 1 {
            // Second is cached, but its next is read later, so the ceiling
            // rather than successor prefetch must exclude this new action.
            appended_token = actions.publish_shared(&mut appended);
        }
    });
    assert_eq!(calls, 2);
    assert_eq!(seen, [first_token.id(), second_token.id(), 0]);
    assert!(appended_token.is_some());
    assert!(remove(&mut actions, Some(first_token)).is_some());
    assert!(remove(&mut actions, Some(second_token)).is_some());
    assert!(remove(&mut actions, appended_token).is_some());
}

#[def_test]
fn captured_head_survives_unlink_before_traversal_and_republication() {
    let owner = handler();
    let mut actions = ActionSet::new();
    let mut regular = PreparedAction::regular(owner.clone()).unwrap();
    assert!(actions.publish_regular(&mut regular));
    // SAFETY: the detached owner below is retained until delayed traversal
    // finishes; the private set also outlives both scoped snapshots.
    let old = unsafe { actions.snapshot() };
    let retired = actions.withdraw(None).unwrap();
    let replacement = handler();
    let mut regular = PreparedAction::regular(replacement.clone()).unwrap();
    assert!(actions.publish_regular(&mut regular));
    let mut saw_old = false;
    old.with_actions(|action| saw_old = Arc::ptr_eq(action.primary(), &owner));
    assert!(saw_old);
    let mut saw_new = false;
    // SAFETY: no mutation or reclamation occurs during this private traversal.
    unsafe { actions.snapshot() }.with_actions(|action| {
        saw_new = Arc::ptr_eq(action.primary(), &replacement);
    });
    assert!(saw_new);
    // SAFETY: both snapshots are consumed; no other admission or reader exists.
    drop(unsafe { retired.reclaim() });
    assert_eq!(Arc::strong_count(&owner), 1);
    assert!(remove(&mut actions, None).is_some());
    assert_eq!(Arc::strong_count(&replacement), 1);
}

#[def_test]
fn traversal_keeps_withdrawn_cached_successor_until_reclamation() {
    let mut actions = ActionSet::new();
    let mut first = PreparedAction::shared(handler()).unwrap();
    let first_token = actions.publish_shared(&mut first).unwrap();
    let owner = handler();
    let mut second = PreparedAction::shared(owner.clone()).unwrap();
    let second_token = actions.publish_shared(&mut second).unwrap();
    let mut third = PreparedAction::shared(handler()).unwrap();
    let third_token = actions.publish_shared(&mut third).unwrap();
    // SAFETY: detached second remains owned until both traversals finish;
    // linked first/third and the private set likewise remain alive.
    let snapshot = unsafe { actions.snapshot() };
    let mut seen = [0; 3];
    let mut calls = 0;
    let mut retained_during_read = false;
    let mut retired = None;
    snapshot.with_actions(|action| {
        seen[calls] = action.token().id();
        calls += 1;
        if action.token() == first_token {
            retired = actions.withdraw(Some(second_token));
            retained_during_read = retired.is_some() && Arc::strong_count(&owner) == 2;
        }
    });
    let mut new_seen = [0; 2];
    let mut new_calls = 0;
    // SAFETY: all linked nodes are retained through this private traversal.
    unsafe { actions.snapshot() }.with_actions(|action| {
        new_seen[new_calls] = action.token().id();
        new_calls += 1;
    });
    // SAFETY: both snapshots are consumed and no other reader/admission exists.
    let removed = unsafe { retired.unwrap().reclaim() };
    assert!(retained_during_read);
    assert_eq!(calls, 3);
    assert_eq!(
        seen,
        [first_token.id(), second_token.id(), third_token.id()]
    );
    assert_eq!(new_calls, 2);
    assert_eq!(new_seen, [first_token.id(), third_token.id()]);
    assert_eq!(actions.len(), 2);
    assert_eq!(removed.token(), second_token);
    assert_eq!(Arc::strong_count(&owner), 2);
    drop(removed);
    assert_eq!(Arc::strong_count(&owner), 1);
    assert!(remove(&mut actions, Some(first_token)).is_some());
    assert!(remove(&mut actions, Some(third_token)).is_some());
}

#[def_test]
fn publication_sequences_never_wrap_even_after_regular_removal() {
    let mut actions = ActionSet::new();
    actions.sequence = usize::MAX - 1;
    let mut regular = PreparedAction::regular(handler()).unwrap();
    assert!(actions.publish_regular(&mut regular));
    assert_eq!(actions.sequence, usize::MAX);
    assert!(remove(&mut actions, None).is_some());
    assert!(!actions.can_install_regular());
    assert!(!actions.can_install_shared());
    let mut regular = PreparedAction::regular(handler()).unwrap();
    let mut shared = PreparedAction::shared(handler()).unwrap();
    assert!(!actions.publish_regular(&mut regular));
    assert!(actions.publish_shared(&mut shared).is_none());
    assert!(regular.pending.is_some() && shared.pending.is_some());
    assert_eq!(actions.len(), 0);
    assert_eq!(actions.sequence, usize::MAX);
    let mut shared_actions = ActionSet::new();
    shared_actions.sequence = usize::MAX - 1;
    let token = shared_actions.publish_shared(&mut shared).unwrap();
    assert_eq!(token.id(), usize::MAX);
    assert!(remove(&mut shared_actions, Some(token)).is_some());
    assert!(!shared_actions.can_install_regular());
    assert!(!shared_actions.can_install_shared());
}
