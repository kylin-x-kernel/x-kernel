// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Borrowed dispatch: callbacks run outside the timer lock; storage stays external.

use intrusive_collections::UnsafeRef;
use kspin::SpinNoIrq;

use super::cpu::CpuTimers;

/// Drains one fixed-cutoff round, completing each delivery before taking the next.
/// Returns false when nothing can be delivered. This is not a hardware IRQ loop.
/// Callback-local resources retire before clearing running identity; the
/// prepared handler and its captures remain externally owned for future calls.
/// No callback is called while holding the timer lock.
///
/// # Safety
/// The caller services the correct CPU and keeps every participating node fixed
/// and live through pending, running and all external accesses. All node changes
/// follow the same owner-lock protocol. The callback must not free its node or
/// synchronously wait for this delivery; it may rearm or try-cancel under the lock.
/// The bound handler must suit the calling context, must not
/// unwind, and must not retain node references beyond the call.
/// Each node's with_handler storage contract must remain valid through dispatch.
pub(super) unsafe fn dispatch_round(timers: &SpinNoIrq<CpuTimers>, due_at_ns: u64) -> bool {
    let mut expired = {
        let mut timers = timers.lock();
        timers.take_expired(due_at_ns)
    };
    let any_due = expired.is_some();
    while let Some((node, expiration)) = expired {
        // SAFETY: take_expired published running and the lock was released.
        // The caller preserves node and bound handler storage through delivery.
        unsafe { node.invoke(timers, expiration) };
        let mut timers = timers.lock();
        let address = UnsafeRef::into_raw(node).addr();
        // SAFETY: callback cleanup and all references to this delivery have ended.
        // No access through the old node handle follows publication. A successor
        // can only be borrowed again by take_expired under this same owner lock.
        unsafe { timers.clear_running(address) };
        expired = timers.take_expired(due_at_ns);
    }
    any_due
}

#[cfg(unittest)]
mod tests {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use ktime_types::{MonotonicInstant, TimeSpan};
    use unittest::{assert, assert_eq, def_test};

    use super::*;
    use crate::{
        control::TimerControl,
        deadline::{Deadline, Expiration},
        index::{TimerHandler, TimerNode},
    };

    fn instant(ns: u64) -> MonotonicInstant {
        MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(ns))
    }
    fn setting(ns: u64) -> Deadline {
        Deadline::new(instant(ns)).unwrap()
    }

    #[def_test]
    fn bound_callback_rearms_and_then_stops() {
        let timers = SpinNoIrq::new(CpuTimers::new());
        let observed = AtomicUsize::new(0);
        let callback = |timer: TimerControl<'_>, event: Expiration| {
            observed.store(event.deadline_ns as usize, Ordering::Relaxed);
            if event.deadline_ns == 10 {
                timer.rearm(instant(20)).unwrap();
            }
        };
        // SAFETY: stack-stable callback outlives the node and all deliveries.
        let node = unsafe { TimerNode::with_handler(setting(10), &callback) };
        // SAFETY: stable node, exclusively accessed through this CPU lock.
        unsafe { timers.lock().arm(&node, setting(10)) };
        // SAFETY: sole modeled dispatcher with retained node and callback storage.
        assert!(unsafe { dispatch_round(&timers, 10) });
        assert_eq!(observed.load(Ordering::Relaxed), 10);
        assert_eq!(timers.lock().earliest_deadline_ns(), Some(20));
        // SAFETY: unchanged storage and serialized CPU service.
        assert!(unsafe { dispatch_round(&timers, 20) });
        assert_eq!(observed.load(Ordering::Relaxed), 20);
        assert!(!node.is_linked());
        assert!(!timers.lock().is_running(&node));
    }

    #[def_test]
    fn callback_local_cleanup_can_rearm_before_running_clear() {
        struct Cleanup<'a>(TimerControl<'a>);
        impl Drop for Cleanup<'_> {
            fn drop(&mut self) {
                self.0.rearm(instant(20)).unwrap();
            }
        }
        let timers = SpinNoIrq::new(CpuTimers::new());
        let callback = |timer: TimerControl<'_>, event: Expiration| {
            if event.deadline_ns == 10 {
                let _cleanup = Cleanup(timer);
            }
        };
        // SAFETY: stable callback and captures outlive every node access.
        let node = unsafe { TimerNode::with_handler(setting(10), &callback) };
        // SAFETY: fixed node, one collection, accesses under the owner lock.
        unsafe { timers.lock().arm(&node, setting(10)) };
        // SAFETY: callback cleanup only takes this unlocked owner lock.
        assert!(unsafe { dispatch_round(&timers, 10) });
        assert_eq!(timers.lock().earliest_deadline_ns(), Some(20));
        // SAFETY: retained storage, sequential second delivery with no rearm.
        assert!(unsafe { dispatch_round(&timers, 20) });
        assert!(!node.is_linked());
    }

    #[def_test]
    fn invalid_callback_replacement_preserves_explicit_successor() {
        let timers = SpinNoIrq::new(CpuTimers::new());
        let rejected = AtomicUsize::new(0);
        let beyond = MonotonicInstant::from_span_since_origin(
            TimeSpan::try_from_nanos(u128::from(u64::MAX) + 1).unwrap(),
        );
        let callback = |timer: TimerControl<'_>, event: Expiration| {
            if event.deadline_ns == 10 {
                timer.rearm(instant(20)).unwrap();
                rejected.store(usize::from(timer.rearm(beyond).is_err()), Ordering::Relaxed);
            }
        };
        // SAFETY: callback storage is fixed and live through both deliveries.
        let node = unsafe { TimerNode::with_handler(setting(10), &callback) };
        // SAFETY: stable node exclusively accessed under this CPU's lock.
        unsafe { timers.lock().arm(&node, setting(10)) };
        // SAFETY: serialized modeled dispatch, callbacks only use scoped rearm.
        assert!(unsafe { dispatch_round(&timers, 10) });
        assert_eq!(timers.lock().earliest_deadline_ns(), Some(20));
        assert_eq!(rejected.load(Ordering::Relaxed), 1);
        // SAFETY: unchanged storage contract, final delivery drains the node.
        assert!(unsafe { dispatch_round(&timers, 20) });
        assert!(!node.is_linked());
    }

    #[def_test]
    fn borrowed_dispatch_does_not_clone_external_owners() {
        use alloc::sync::Arc;
        struct Handler(AtomicUsize);
        impl TimerHandler for Handler {
            fn on_expiration(&self, _: TimerControl<'_>, _: Expiration) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let handler = Arc::new(Handler(AtomicUsize::new(0)));
        // SAFETY: stable Arc storage remains live until the node is drained.
        let node = Arc::new(unsafe { TimerNode::with_handler(setting(10), handler.as_ref()) });
        let timers = SpinNoIrq::new(CpuTimers::new());
        for ns in [10, 60] {
            // SAFETY: both Arcs remain externally owned, node is idle between rounds.
            unsafe { timers.lock().arm(&node, setting(ns)) };
            // SAFETY: sole modeled dispatcher, retained node and handler storage.
            assert!(unsafe { dispatch_round(&timers, ns) });
            assert_eq!(Arc::strong_count(&node), 1);
            assert_eq!(Arc::strong_count(&handler), 1);
            assert!(!node.is_linked());
        }
        assert_eq!(handler.0.load(Ordering::Relaxed), 2);
    }

    #[def_test]
    fn one_tree_dispatches_distinct_bound_handlers() {
        let timers = SpinNoIrq::new(CpuTimers::new());
        let first = AtomicUsize::new(0);
        let second = AtomicUsize::new(0);
        let a = |_: TimerControl<'_>, _: Expiration| {
            first.fetch_add(1, Ordering::Relaxed);
        };
        let b = |_: TimerControl<'_>, _: Expiration| {
            second.fetch_add(2, Ordering::Relaxed);
        };
        // SAFETY: both fixed closures outlive the array and all its memberships.
        let nodes = unsafe {
            [
                TimerNode::with_handler(setting(10), &a),
                TimerNode::with_handler(setting(20), &b),
            ]
        };
        for (i, node) in nodes.iter().enumerate() {
            // SAFETY: fixed node array and callbacks; serialized owner-lock access.
            unsafe { timers.lock().arm(node, setting(10 + i as u64 * 10)) };
        }
        // SAFETY: sole modeled dispatcher, handlers remain valid throughout.
        assert!(unsafe { dispatch_round(&timers, 10) });
        assert_eq!(first.load(Ordering::Relaxed), 1);
        assert_eq!(second.load(Ordering::Relaxed), 0);
        // SAFETY: same live bindings; final node is drained by this delivery.
        assert!(unsafe { dispatch_round(&timers, 20) });
        assert_eq!(second.load(Ordering::Relaxed), 2);
        assert!(nodes.iter().all(|node| !node.is_linked()));
    }

    #[def_test]
    fn empty_dispatch_has_no_delivery() {
        let timers = SpinNoIrq::new(CpuTimers::new());
        // SAFETY: empty collection with no concurrent dispatcher or borrowed nodes.
        assert!(!unsafe { dispatch_round(&timers, 10) });
    }
}
