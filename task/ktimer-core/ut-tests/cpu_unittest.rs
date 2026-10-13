// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

    use ktime_types::{MonotonicInstant, TimeSpan};
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    fn setting(ns: u64) -> Deadline {
        Deadline::new(MonotonicInstant::from_span_since_origin(
            TimeSpan::from_nanos(ns),
        ))
        .unwrap()
    }

    /// # Safety
    /// The caller ended all callback resources and node accesses for this delivery.
    unsafe fn complete(timers: &mut CpuTimers, node: UnsafeRef<TimerNode>) {
        let address = UnsafeRef::into_raw(node).addr();
        // SAFETY: caller finished this delivery; converting the handle ends its borrow.
        unsafe { timers.clear_running(address) };
    }

    #[def_test]
    fn one_shot_consumes_membership_without_implicit_successor() {
        let node = TimerNode::new(setting(10));
        let mut timers = CpuTimers::new();
        // SAFETY: stack-stable node outlives this sequentially accessed collection.
        unsafe { timers.arm(&node, setting(10)) };
        assert!(timers.take_expired(9).is_none());
        let (running, event) = timers.take_expired(35).unwrap();
        assert_eq!(event.deadline_ns, 10);
        assert_eq!(timers.earliest_deadline_ns(), None);
        assert!(timers.take_expired(35).is_none());
        // SAFETY: no callback or borrowed resources remain for this delivery.
        unsafe { complete(&mut timers, running) };
        assert!(timers.take_expired(u64::MAX - 1).is_none());
    }

    #[def_test]
    fn running_replacements_preserve_snapshot_and_cleanup_barrier() {
        let node = TimerNode::new(setting(10));
        let mut timers = CpuTimers::new();
        // SAFETY: stable local storage, all access serialized by this test.
        unsafe { timers.arm(&node, setting(10)) };
        let (running, event) = timers.take_expired(10).unwrap();
        // SAFETY: same running node and collection; successor uses its existing link.
        assert!(unsafe { timers.arm(&node, setting(20)) });
        // SAFETY: The same stack-stable node is accessed only by this test,
        // and its existing membership belongs to timers.
        assert!(!unsafe { timers.arm(&node, setting(30)) });
        // SAFETY: timers owns this node's membership; access remains serialized.
        let canceled = unsafe { timers.try_cancel(&node) };
        assert!(matches!(canceled, CancelResult::Running));
        assert_eq!(event.deadline_ns, 10);
        assert_eq!(timers.earliest_deadline_ns(), Some(30));
        assert!(timers.take_expired(100).is_none());
        // SAFETY: callback resources and delivery references end before publication.
        unsafe { complete(&mut timers, running) };
        let (next, event) = timers.take_expired(100).unwrap();
        assert_eq!(event.deadline_ns, 30);
        // SAFETY: second modeled delivery has no remaining callback resources.
        unsafe { complete(&mut timers, next) };
        assert!(timers.take_expired(100).is_none());
    }

    #[def_test]
    fn cancellation_retries_after_cleanup_with_or_without_successor() {
        for has_successor in [false, true] {
            let node = TimerNode::new(setting(10));
            let mut timers = CpuTimers::new();
            // SAFETY: fixed stack node outlives all sequential tree accesses.
            unsafe { timers.arm(&node, setting(10)) };
            let (running, _) = timers.take_expired(10).unwrap();
            if has_successor {
                // SAFETY: same live running node; no concurrent access.
                unsafe { timers.arm(&node, setting(20)) };
            }
            assert!(matches!(
                // SAFETY: this collection owns the node and current delivery.
                unsafe { timers.try_cancel(&node) },
                CancelResult::Running
            ));
            assert!(timers.take_expired(20).is_none());
            // SAFETY: modeled callback cleanup has ended.
            unsafe { complete(&mut timers, running) };
            // SAFETY: node remains stable and belongs only to this collection.
            match unsafe { timers.try_cancel(&node) } {
                CancelResult::Removed(removed) => {
                    assert!(has_successor);
                    assert!(core::ptr::eq(removed.as_ref(), &node));
                }
                CancelResult::Inactive => assert!(!has_successor),
                CancelResult::Running => assert!(false),
            }
            assert_eq!(timers.earliest_deadline_ns(), None);
        }
    }

    #[def_test]
    fn pending_cancel_removes_only_target_and_idle_can_restart() {
        let nodes = [TimerNode::new(setting(10)), TimerNode::new(setting(10))];
        let mut timers = CpuTimers::new();
        for node in &nodes {
            // SAFETY: fixed array outlives the collection; sequential access only.
            unsafe { timers.arm(node, setting(10)) };
        }
        assert!(matches!(
            // SAFETY: exact member of this collection.
            unsafe { timers.try_cancel(&nodes[0]) },
            CancelResult::Removed(_)
        ));
        let (running, _) = timers.take_expired(10).unwrap();
        assert!(core::ptr::eq(running.as_ref(), &nodes[1]));
        // SAFETY: first modeled delivery has completed, with no retained borrows.
        unsafe { complete(&mut timers, running) };
        // SAFETY: canceled node remains stable and can be reused.
        unsafe { timers.arm(&nodes[0], setting(20)) };
        let (running, event) = timers.take_expired(20).unwrap();
        assert_eq!(event.deadline_ns, 20);
        // SAFETY: no callback resources remain in the second delivery.
        unsafe { complete(&mut timers, running) };
    }

    #[def_test]
    fn backoff_only_changes_service_deadline() {
        let node = TimerNode::new(setting(10));
        let mut timers = CpuTimers::new();
        // SAFETY: stack-stable node outlives this sequential collection.
        unsafe { timers.arm(&node, setting(10)) };
        timers.defer_dispatch_until(50);
        assert_eq!(timers.earliest_deadline_ns(), Some(10));
        assert_eq!(timers.service_deadline_ns(), Some(50));
        assert!(!timers.can_dispatch(49));
        assert!(timers.can_dispatch(50));
        assert_eq!(timers.service_deadline_ns(), Some(10));
        // SAFETY: still-live member of this collection, not running.
        unsafe { timers.try_cancel(&node) };
        assert_eq!(timers.service_deadline_ns(), None);
    }

    #[def_test]
    fn external_storage_is_released_after_running_clear() {
        let node = alloc::boxed::Box::new(TimerNode::new(setting(10)));
        let mut timers = CpuTimers::new();
        // SAFETY: allocation remains live through running retirement.
        unsafe { timers.arm(&node, setting(10)) };
        let (running, _) = timers.take_expired(10).unwrap();
        assert!(timers.take_expired(20).is_none());
        // SAFETY: no callback resources remain and owner storage is still live.
        unsafe { complete(&mut timers, running) };
        drop(node);
        assert!(timers.running.is_none());
        assert!(timers.take_expired(20).is_none());
    }