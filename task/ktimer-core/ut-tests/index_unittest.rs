// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

    use core::ptr;

    use ktime_types::{MonotonicInstant, TimeSpan};
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    fn setting(deadline_ns: u64) -> Deadline {
        Deadline::new(MonotonicInstant::from_span_since_origin(
            TimeSpan::from_nanos(deadline_ns),
        ))
        .unwrap()
    }

    fn cache_is_valid(index: &TimerIndex) -> bool {
        let actual = index.tree.front().get().map(|node| {
            // SAFETY: this test reads linked deadlines without changing them.
            unsafe { node.deadline().deadline_ns() }
        });
        index.earliest == index.tree.front().get().map(NonNull::from)
            && index.earliest_deadline_ns() == actual
    }

    #[def_test]
    fn duplicate_deadlines_remove_by_identity_and_preserve_minimum() {
        let nodes = [
            TimerNode::new(setting(20)),
            TimerNode::new(setting(10)),
            TimerNode::new(setting(10)),
        ];
        let mut index = TimerIndex::new();
        assert_eq!(index.earliest_deadline_ns(), None);
        for node in &nodes {
            // SAFETY: nodes remain at their stack addresses until after index
            // drops; each is unlinked and this test is their sole accessor.
            let pending = unsafe { UnsafeRef::from_raw(node) };
            // SAFETY: This test keeps the unlinked node stable and exclusively
            // accessed until the index removes it or is dropped.
            unsafe { index.insert(pending) };
            assert!(cache_is_valid(&index));
        }
        // SAFETY: nodes[2] was inserted in this index above and not removed.
        let removed = unsafe { index.remove(&nodes[2]) };
        assert!(ptr::eq(removed.as_ref(), &nodes[2]));
        assert!(!removed.link.is_linked());
        assert!(cache_is_valid(&index));
        assert!(index.pop_due(9).is_none());
        assert!(ptr::eq(index.pop_due(10).unwrap().as_ref(), &nodes[1]));
        assert!(cache_is_valid(&index));
        assert!(index.pop_due(19).is_none());
        assert!(ptr::eq(index.pop_due(20).unwrap().as_ref(), &nodes[0]));
        assert!(cache_is_valid(&index));
        assert!(index.pop_due(u64::MAX).is_none());
    }

    #[def_test]
    fn removed_node_can_change_deadline_and_reuse_its_link() {
        let node = TimerNode::new(setting(100));
        let mut index = TimerIndex::new();
        for deadline in [100, 0, u64::MAX, 10, 1] {
            // SAFETY: the previous iteration drained this node. This test
            // exclusively accesses it, and its address remains stable.
            unsafe { node.replace_deadline(setting(deadline)) };
            // SAFETY: The stack node stays live and unmoved until this index
            // and all removed handles have finished accessing it.
            let pending = unsafe { UnsafeRef::from_raw(&node) };
            // SAFETY: The node is unlinked, has its prepared deadline, and this
            // sequential test excludes all concurrent node access.
            unsafe { index.insert(pending) };
            assert_eq!(index.earliest_deadline_ns(), Some(deadline));
            if deadline > 0 {
                assert!(index.pop_due(deadline - 1).is_none());
            }
            let removed = index.pop_due(deadline).unwrap();
            assert!(ptr::eq(removed.as_ref(), &node));
            assert!(!node.link.is_linked());
            assert!(cache_is_valid(&index));
        }
    }

    #[def_test]
    fn index_drop_unlinks_but_does_not_destroy_borrowed_nodes() {
        let node = TimerNode::new(setting(7));
        {
            let mut index = TimerIndex::new();
            // SAFETY: node outlives this index and is neither moved nor
            // modified while the index (including its Drop) accesses it.
            let pending = unsafe { UnsafeRef::from_raw(&node) };
            // SAFETY: This test keeps the unlinked node stable and exclusively
            // accessed until the index removes it or is dropped.
            unsafe { index.insert(pending) };
        }
        assert!(!node.link.is_linked());
        // SAFETY: index destruction finished; no other node access exists.
        let deadline = unsafe { node.deadline().deadline_ns() };
        assert_eq!(deadline, 7);
    }

    #[def_test]
    fn repeated_reordering_matches_reference_minimum() {
        let nodes: [TimerNode; 64] = core::array::from_fn(|i| TimerNode::new(setting(i as u64)));
        let mut index = TimerIndex::new();
        let mut deadlines = [None; 64];
        for step in 0..2048 {
            let slot = (step * 17) % nodes.len();
            if deadlines[slot].is_some() {
                // SAFETY: the reference table records membership in index;
                // there is no other access to these stack-stable nodes.
                let removed = unsafe { index.remove(&nodes[slot]) };
                assert!(ptr::eq(removed.as_ref(), &nodes[slot]));
            }
            let deadline = ((step * 31) % 127) as u64;
            // SAFETY: this node is now unlinked, has a stable stack address,
            // and is exclusively accessed by this sequential test.
            unsafe { nodes[slot].replace_deadline(setting(deadline)) };
            // SAFETY: The stack node stays live and unmoved until this index
            // and all removed handles have finished accessing it.
            let pending = unsafe { UnsafeRef::from_raw(&nodes[slot]) };
            // SAFETY: The node is unlinked, has its prepared deadline, and this
            // sequential test excludes all concurrent node access.
            unsafe { index.insert(pending) };
            deadlines[slot] = Some(deadline);
            assert_eq!(
                index.earliest_deadline_ns(),
                deadlines.iter().flatten().copied().min()
            );
            assert!(cache_is_valid(&index));
        }
        let mut previous = 0;
        let mut drained = 0;
        while let Some(node) = index.pop_due(u64::MAX) {
            // SAFETY: removed node remains backed by nodes; no writer exists.
            let deadline = unsafe { node.deadline().deadline_ns() };
            assert!(deadline >= previous);
            previous = deadline;
            drained += 1;
            assert!(cache_is_valid(&index));
        }
        assert_eq!(drained, nodes.len());
        assert!(nodes.iter().all(|node| !node.link.is_linked()));
    }