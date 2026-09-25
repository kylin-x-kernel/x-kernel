// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{assert, assert_eq, pin::pin, ptr::NonNull};

use unittest::def_test;

// Use terminal kernel assertions so a failed check cannot return while another
// live list still borrows the local node storage.
use super::{PendingList, PendingNode};

/// Checks FIFO removal and repeated reuse of prepared, stack-pinned nodes.
///
/// # Panics
/// Panics if order changes, membership is lost or an empty list yields a node.
#[def_test]
fn fifo_and_reuse() {
    let first = pin!(PendingNode::new());
    let second = pin!(PendingNode::new());
    let mut pending = PendingList::new();
    assert!(pending.is_empty());
    assert!(pending.pop_first_matching(|_| true).is_none());
    for _ in 0..64 {
        // SAFETY: both pinned nodes outlive the list; all accesses are shared
        // and list/link mutations are serialized in this test until drained.
        unsafe {
            assert!(pending.push_back(NonNull::from(&*first)));
            assert!(pending.push_back(NonNull::from(&*second)));
        }
        assert_eq!(
            pending.pop_first_matching(|_| true),
            Some(NonNull::from(&*first))
        );
        assert!(!first.is_linked());
        assert!(second.is_linked());
        assert_eq!(
            pending.pop_first_matching(|_| true),
            Some(NonNull::from(&*second))
        );
        assert!(pending.is_empty());
        assert!(!second.is_linked());
    }
}

/// Checks skipping a blocked predecessor does not move it behind later arrivals.
///
/// # Panics
/// Panics if eligibility scanning removes or reorders skipped entries.
#[def_test]
fn skipped_nodes_keep_fifo_position() {
    let first = pin!(PendingNode::new());
    let second = pin!(PendingNode::new());
    let third = pin!(PendingNode::new());
    let mut pending = PendingList::new();
    // SAFETY: stable pinned storage outlives the drained list, with only shared
    // node access and serialized linkage operations.
    unsafe {
        assert!(pending.push_back(NonNull::from(&*first)));
        assert!(pending.push_back(NonNull::from(&*second)));
    }
    assert!(pending.pop_first_matching(|_| false).is_none());
    let second_address = NonNull::from(&*second);
    assert_eq!(
        pending.pop_first_matching(|node| node == second_address),
        Some(second_address)
    );
    assert!(first.is_linked());
    // SAFETY: third has the same stable lifetime and serialized access as above.
    assert!(unsafe { pending.push_back(NonNull::from(&*third)) });
    assert_eq!(
        pending.pop_first_matching(|_| true),
        Some(NonNull::from(&*first))
    );
    assert_eq!(
        pending.pop_first_matching(|_| true),
        Some(NonNull::from(&*third))
    );
    assert!(pending.is_empty());
}

/// Checks direct middle removal and duplicate insertion leave other entries intact.
///
/// # Panics
/// Panics if duplicate insertion succeeds or cancellation damages FIFO membership.
#[def_test]
fn direct_removal_and_duplicate_rejection() {
    let first = pin!(PendingNode::new());
    let middle = pin!(PendingNode::new());
    let last = pin!(PendingNode::new());
    let mut pending = PendingList::new();
    let mut other = PendingList::new();
    // SAFETY: nodes stay pinned/alive beyond both lists. Operations on both
    // lists are serialized; removal targets a known member of pending only.
    unsafe {
        assert!(pending.push_back(NonNull::from(&*first)));
        assert!(pending.push_back(NonNull::from(&*middle)));
        assert!(pending.push_back(NonNull::from(&*last)));
        assert!(!pending.push_back(NonNull::from(&*middle)));
        assert!(!other.push_back(NonNull::from(&*middle)));
        assert!(pending.remove(&middle));
        assert!(!pending.remove(&middle));
        assert!(other.push_back(NonNull::from(&*middle)));
    }
    assert_eq!(
        pending.pop_first_matching(|_| true),
        Some(NonNull::from(&*first))
    );
    assert_eq!(
        pending.pop_first_matching(|_| true),
        Some(NonNull::from(&*last))
    );
    assert_eq!(
        other.pop_first_matching(|_| true),
        Some(NonNull::from(&*middle))
    );
    assert!(pending.is_empty());
    assert!(other.is_empty());
}

/// Checks list destruction unlinks borrowed nodes without destroying their storage.
///
/// # Panics
/// Panics if a live external node remains linked after its list is dropped.
#[def_test]
fn list_drop_only_unlinks() {
    let node = pin!(PendingNode::new());
    {
        let mut pending = PendingList::new();
        // SAFETY: pinned node stays live beyond list drop; no concurrent mutation.
        assert!(unsafe { pending.push_back(NonNull::from(&*node)) });
        assert!(node.is_linked());
    }
    assert!(!node.is_linked());
}
