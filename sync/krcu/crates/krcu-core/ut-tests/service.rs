// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicPtr, AtomicUsize};

use intrusive_collections::container_of;
use unittest::{assert, assert_eq};

use super::*;
use crate::callback::CallbackNode;

#[repr(C)]
struct Retirement {
    node: CallbackNode,
    calls: Arc<AtomicUsize>,
}

/// # Safety
/// `node` must be the unique detached node from a submitted Retirement allocation.
unsafe fn invoke_retirement(node: *mut CallbackNode) {
    // SAFETY: the fixture submits a raw field projection of this concrete type.
    let allocation = unsafe { container_of!(node, Retirement, node) }.cast_mut();
    // SAFETY: callback execution uniquely owns the original Box allocation.
    let retirement = unsafe { Box::from_raw(allocation) };
    retirement.calls.fetch_add(1, Ordering::Relaxed);
}

#[repr(C)]
struct PublishedValue {
    node: CallbackNode,
    value: u64,
    drops: Arc<AtomicUsize>,
}

impl Drop for PublishedValue {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

/// # Safety
/// `node` is the detached field projection of a uniquely submitted PublishedValue
/// Box. All fixture borrows have ended; invocation consumes that allocation once.
unsafe fn reclaim_published_value(node: *mut CallbackNode) {
    // SAFETY: the submitted projection belongs to this concrete container.
    let allocation = unsafe { container_of!(node, PublishedValue, node) }.cast_mut();
    // SAFETY: retirement transferred the original Box owner; no borrow remains.
    drop(unsafe { Box::from_raw(allocation) });
}

#[unittest::def_test]
fn interrupt_borrow_delays_retirement_until_quiescent_return() {
    let nodes = [ReaderNode::new()];
    let watching = crate::CpuWatching::new();
    let reader = TaskReader::new();
    // SAFETY: this fixture exclusively drives its isolated nodes and CPU model.
    let state = unsafe { CallbackState::new(&nodes, [true]) }.unwrap();
    let mut driver = state.claim_driver().unwrap();
    let drops = Arc::new(AtomicUsize::new(0));
    let allocation = Box::into_raw(Box::new(PublishedValue {
        node: CallbackNode::new(reclaim_published_value),
        value: 17,
        drops: drops.clone(),
    }));
    let published = AtomicPtr::new(allocation);
    // This serial model interleaves a reader CPU and an updater/GP CPU. It does
    // not change real CPU context, run allocations in NMI or inject hardware.
    // SAFETY: exclusively owned CPU model, no outstanding interrupt or borrow.
    unsafe { watching.enter_quiescent() };
    // SAFETY: same modeled CPU, strictly nested entry before protected access.
    let interrupt = unsafe { watching.enter_interrupt() };
    let read_fn = |_: &crate::ReadGuard<'_>| {
        let current = published.load(Ordering::Acquire);
        // SAFETY: initialized allocation remains raw-owned; this test never
        // invokes reclamation while the borrow lives, even on GP regression.
        let old = unsafe { &(*current).value };
        let retired = published.swap(core::ptr::null_mut(), Ordering::AcqRel);
        // SAFETY: projection retains the original Box provenance.
        let node = unsafe { &raw mut (*retired).node };
        // SAFETY: removal precedes submission; the fixture retains storage
        // until callback invocation after the borrow and interrupt end.
        let accepted = unsafe { state.submit(0, node) };
        let started = driver.advance();
        // SAFETY: fresh post-start samples of the matching isolated CPU;
        // the real caller stays watched and holds no bookkeeping lock.
        let first_report = unsafe { state.observe_watching(0, &watching) };
        // SAFETY: repeated fresh sample under the same isolated conditions.
        let second_report = unsafe { state.observe_watching(0, &watching) };
        (
            accepted,
            started,
            first_report || second_report,
            driver.advance(),
            state.has_ready(0),
            *old,
            drops.load(Ordering::Relaxed),
        )
    };
    // SAFETY: exclusive fixture reader, no enrollment or scheduling. The scope
    // ends before its modeled interrupt; only atomic accounting runs on exit.
    let observed = unsafe { reader.with_read_deferred(read_fn) };
    // Scope accounting alone is not a CPU observation: the interrupt has not
    // returned yet. Check eligibility, not invocation, to keep regressions safe.
    // SAFETY: same isolated watched CPU model, no reader borrow or lock remains.
    let reported_before_return = unsafe { state.observe_watching(0, &watching) };
    let ready_before_return = state.has_ready(0);
    // SAFETY: all modeled interrupt scopes ended on this same simulated stack.
    unsafe { interrupt.exit() };
    // SAFETY: fresh matching observation after the modeled return to EQS.
    let reported_after_return = unsafe { state.observe_watching(0, &watching) };
    let remains_active = driver.advance();
    // SAFETY: no borrow remains, even if eligibility regressed above. This sole
    // executor consumes the original Box via its nonblocking owning callback.
    let invoked = unsafe { state.invoke_ready(0, 1) };
    if !observed.0 {
        // SAFETY: rejection leaves the allocation caller-owned and unqueued;
        // the read scope and modeled interrupt have both finished.
        drop(unsafe { Box::from_raw(allocation) });
    }
    // SAFETY: finish the same modeled quiescent window before assertions.
    unsafe { watching.exit_quiescent() };
    assert_eq!(observed, (true, true, false, true, false, 17, 0));
    assert!(!reader.is_reading() && !reader.is_enrolled());
    assert!(published.load(Ordering::Acquire).is_null());
    assert!(!reported_before_return && !ready_before_return);
    assert!(reported_after_return && !remains_active);
    assert_eq!(invoked, 1);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[unittest::def_test]
fn watching_report_does_not_make_an_old_readers_callback_eligible() {
    let nodes = [ReaderNode::new()];
    let watching = crate::CpuWatching::new();
    let reader = TaskReader::new();
    // SAFETY: isolated nodes with no other controller or real protected object.
    let state = unsafe { CallbackState::new(&nodes, [true]) }.unwrap();
    let mut driver = state.claim_driver().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let object = Box::into_raw(Box::new(Retirement {
        node: CallbackNode::new(invoke_retirement),
        calls: calls.clone(),
    }));
    // SAFETY: raw projection preserves the containing Box provenance.
    let node = unsafe { &raw mut (*object).node };
    // SAFETY: unique unpublished fixture, retained until its owning nonblocking
    // callback. This fixture drives the service through completion below.
    assert!(unsafe { state.submit(0, node) });
    let blocked = reader.with_read(|_| {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: isolated stable reader; only this simulated scheduler enrolls
        // it, under exclusion. Scope exit removes it before storage is dropped.
        unsafe { nodes[0].enroll(&reader) };
        let started = driver.advance();
        // SAFETY: isolated CPU fixture now models idle after the reader was
        // switched out. No actual CPU transition or protected access is faked.
        unsafe { watching.enter_quiescent() };
        // SAFETY: watching matches this isolated node, host remains watched,
        // no bookkeeping lock is held and no real object relies on the result.
        let reported = unsafe { state.observe_watching(0, &watching) };
        // SAFETY: close the same fixture's quiescent window before assertions.
        unsafe { watching.exit_quiescent() };
        (
            started,
            reported,
            state.needs_cpu_observation(0),
            driver.advance(),
            state.has_ready(0),
        )
    });
    // SAFETY: fresh watched readiness inspection after the retained reader's
    // outer exit. This supplies no additional CPU observation.
    let reported_after_exit = unsafe { state.report_ready(0) };
    let remains_active = driver.advance();
    // SAFETY: isolated unpublished Box, reader removed and modeled GP complete;
    // its callback owns the allocation and no bookkeeping lock is held.
    let invoked = unsafe { state.invoke_ready(0, 1) };
    assert_eq!(blocked, (true, false, false, true, false));
    assert!(reported_after_exit);
    assert!(!remains_active);
    assert_eq!(invoked, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[unittest::def_test]
fn driver_claim_is_not_released_by_drop_or_move() {
    let nodes = [ReaderNode::new()];
    // SAFETY: isolated nodes have no other controller or active period.
    let state = unsafe { CallbackState::new(&nodes, [true]) }.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let object = Box::into_raw(Box::new(Retirement {
        node: CallbackNode::new(invoke_retirement),
        calls: calls.clone(),
    }));
    let executed = {
        let mut driver = state.claim_driver().unwrap();
        // SAFETY: raw projection retains the concrete Box provenance.
        let node = unsafe { &raw mut (*object).node };
        // SAFETY: unpublished fixture with no external readers; state and storage
        // remain live through invocation. The callback owns the original Box and
        // the test drives this isolated service to completion below.
        assert!(unsafe { state.submit(0, node) });
        assert!(driver.advance());
        assert!(state.claim_driver().is_none());
        let mut successor = driver;
        assert!(state.claim_driver().is_none());
        assert_eq!(calls.load(Ordering::Relaxed), 0);
        // Controlled observation for an unpublished test object, not real hardware
        // quiescence. Only this driver can advance the coordinator in this fixture.
        let period = state.coordinator.completed_period().wrapping_add(1);
        assert!(state.coordinator.observe_cpu(0, period));
        assert!(state.coordinator.report_node(0));
        assert!(!successor.advance());
        // SAFETY: isolated unpublished object, completed simulated period, sole
        // executor and nonblocking callback, with no bookkeeping lock held.
        unsafe { state.invoke_ready(0, 1) }
    };
    assert!(state.claim_driver().is_none());
    assert_eq!(executed, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    assert_eq!(state.queues[0].outstanding(), 0);
}

#[unittest::def_test]
fn rejected_submission_preserves_node_for_participating_cpu() {
    let nodes = [ReaderNode::new(), ReaderNode::new()];
    // SAFETY: fresh isolated nodes have no other controller.
    let state = unsafe { CallbackState::new(&nodes, [true, false]) }.unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let object = Box::into_raw(Box::new(Retirement {
        node: CallbackNode::new(invoke_retirement),
        calls: calls.clone(),
    }));
    // SAFETY: raw projection retains the original Box provenance.
    let node = unsafe { &raw mut (*object).node };
    // SAFETY: rejection leaves the valid unpublished fixture owned by this test.
    assert!(!unsafe { state.submit(0, node) });
    let mut driver = state.claim_driver().unwrap();
    // SAFETY: each rejected call leaves this unique node unqueued and retained.
    assert!(!unsafe { state.submit(1, node) });
    // SAFETY: out-of-capacity rejection must also preserve the node.
    assert!(!unsafe { state.submit(2, node) });
    assert!(!state.has_ready(1));
    assert!(!state.has_ready(2));
    // SAFETY: unpublished retained fixture, sole control owner, nonblocking Box
    // cleanup and explicit test service driving through execution below.
    assert!(unsafe { state.submit(0, node) });
    assert!(driver.advance());
    let period = state.coordinator.completed_period().wrapping_add(1);
    assert!(state.coordinator.observe_cpu(0, period));
    assert!(state.coordinator.report_node(0));
    assert!(!driver.advance());
    // SAFETY: unpublished fixture has no real readers; its simulated period has
    // completed and no bookkeeping lock is held during the nonblocking callback.
    assert_eq!(unsafe { state.invoke_ready(0, 1) }, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[unittest::def_test]
fn execution_waits_for_promotion_and_preserves_budget_remainder() {
    let nodes = [ReaderNode::new()];
    // SAFETY: this test exclusively controls the isolated node.
    let state = unsafe { CallbackState::new(&nodes, [true]) }.unwrap();
    let mut driver = state.claim_driver().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    for _ in 0..2 {
        let object = Box::into_raw(Box::new(Retirement {
            node: CallbackNode::new(invoke_retirement),
            calls: calls.clone(),
        }));
        // SAFETY: field projection preserves the original allocation provenance.
        let node = unsafe { &raw mut (*object).node };
        // SAFETY: unpublished retained fixture; nonblocking callback owns this
        // allocation, and the test serves both queued objects before returning.
        assert!(unsafe { state.submit(0, node) });
    }
    assert!(driver.advance());
    assert!(!state.has_ready(0));
    // SAFETY: these objects were never published and have no readers; the core
    // must nevertheless withhold execution until the controlled GP completes.
    assert_eq!(unsafe { state.invoke_ready(0, 2) }, 0);
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    let period = state.coordinator.completed_period().wrapping_add(1);
    assert!(state.coordinator.observe_cpu(0, period));
    assert!(state.coordinator.report_node(0));
    assert!(!driver.advance());
    assert!(state.has_ready(0));
    // SAFETY: no real readers, completed simulated GP and no dependent lock;
    // this is the sole executor of retained nonblocking callbacks.
    assert_eq!(unsafe { state.invoke_ready(0, 0) }, 0);
    assert!(state.has_ready(0));
    // SAFETY: same isolated completed period and execution context as above.
    assert_eq!(unsafe { state.invoke_ready(0, 1) }, 1);
    assert!(state.has_ready(0));
    assert_eq!(calls.load(Ordering::Relaxed), 1);
    // SAFETY: the second retained callback has the same completed GP.
    assert_eq!(unsafe { state.invoke_ready(0, 1) }, 1);
    assert!(!state.has_ready(0));
    assert_eq!(calls.load(Ordering::Relaxed), 2);
    assert_eq!(state.queues[0].outstanding(), 0);
}

#[unittest::def_test]
fn abandoned_active_driver_cannot_be_recreated() {
    let nodes = [ReaderNode::new()];
    // SAFETY: isolated nodes and no other controller.
    let state = unsafe { CallbackState::new(&nodes, [true]) }.unwrap();
    {
        let _driver = state.claim_driver().unwrap();
        // Pause a control transaction without queueing owned storage. The test
        // deliberately abandons this active coordinator and never uses it again.
        assert!(state.coordinator.begin_period().is_some());
    }
    assert!(state.claim_driver().is_none());
}

#[unittest::def_test]
fn empty_participants_are_rejected_without_consuming_nodes() {
    let nodes = [ReaderNode::new(), ReaderNode::new()];
    // SAFETY: fresh nodes; the rejected construction does not acquire control.
    assert!(unsafe { CallbackState::new(&nodes, [false, false]) }.is_none());
    // SAFETY: no controller was created by the rejected construction above.
    let state = unsafe { CallbackState::new(&nodes, [true, false]) }.unwrap();
    let mut driver = state.claim_driver().unwrap();
    assert!(!driver.advance());
    assert!(state.coordinator.begin_period().is_some());
    let period = state.coordinator.completed_period().wrapping_add(1);
    assert!(!state.coordinator.observe_cpu(1, period));
    assert!(state.coordinator.observe_cpu(0, period));
    assert!(state.coordinator.report_node(0));
    assert_eq!(state.coordinator.try_complete_period(), Some(period));
}

#[unittest::def_test]
fn startup_preserves_an_enrolled_reader_until_its_outer_exit() {
    let nodes = [ReaderNode::new()];
    let reader = TaskReader::new();
    let outgoing = TaskReader::new();
    let (state, period, observed) = reader.with_read(|_| {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: isolated stable reader/node storage models an early outgoing
        // task, before service construction; no concurrent scope or enrollment.
        unsafe { nodes[0].enroll(&reader) };
        // SAFETY: no coordinator or period exists yet. The already-enrolled
        // reader and its node remain fixed through the outer scope's exit.
        let state = unsafe { CallbackState::new(&nodes, [true]) }.unwrap();
        // Drive the private coordinator directly: no callback driver or queued
        // storage participates in this isolated startup accounting test.
        let period = state.coordinator.begin_period().unwrap();
        // SAFETY: a later inactive outgoing task supplies the CPU observation
        // under exclusion, while the early reader remains suspended.
        let reported = unsafe { state.note_context_switch(0, &outgoing) };
        let observed = (
            reader.is_enrolled_on(&nodes[0]),
            reported,
            state.needs_cpu_observation(0),
            state.coordinator.try_complete_period(),
        );
        (state, period, observed)
    });
    assert_eq!(observed, (true, false, false, None));
    assert!(!reader.is_enrolled());
    assert_eq!(nodes[0].reader_count(), 0);
    // SAFETY: the watched fixture rechecks retained readiness after the early
    // reader exits, without adding another CPU observation or holding a lock.
    assert!(unsafe { state.report_ready(0) });
    assert_eq!(state.coordinator.try_complete_period(), Some(period));
}

#[unittest::def_test]
fn scheduler_observation_keeps_a_migrating_reader_blocking() {
    let nodes = [ReaderNode::new(), ReaderNode::new()];
    // SAFETY: isolated stable nodes have one controller and no active period.
    let state = unsafe { CallbackState::new(&nodes, [true, true]) }.unwrap();
    let mut driver = state.claim_driver().unwrap();
    let calls = Arc::new(AtomicUsize::new(0));
    let object = Box::into_raw(Box::new(Retirement {
        node: CallbackNode::new(invoke_retirement),
        calls: calls.clone(),
    }));
    // SAFETY: preserve the enclosing allocation's raw provenance.
    let node = unsafe { &raw mut (*object).node };
    // SAFETY: unique unpublished object retained until nonblocking callback;
    // this isolated service is driven to completion below.
    assert!(unsafe { state.submit(0, node) });
    let reader = TaskReader::new();
    let observed = reader.with_read(|_| {
        let started = driver.advance();
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: this isolated fixture models the outgoing task on CPU 0 with
        // IRQ/preemption excluded. Its reader and nodes outlive unlinking.
        let first_report = unsafe { state.note_context_switch(0, &reader) };
        // SAFETY: serial migration of the same fixture to CPU 1, with no
        // intervening scope mutation or concurrent scheduler access.
        let migrated_report = unsafe { state.note_context_switch(1, &reader) };
        (
            (started, first_report, migrated_report),
            (reader.is_enrolled_on(&nodes[0]), nodes[1].reader_count()),
            (
                driver.advance(),
                state.has_ready(0),
                calls.load(Ordering::Relaxed),
            ),
        )
    });
    assert_eq!(observed, ((true, false, true), (true, 0), (true, false, 0)));
    assert!(!reader.is_enrolled());
    {
        let _guard = kspin::NoPreemptIrqSave::new();
        // SAFETY: isolated watched fixture inspects retained readiness after
        // the prior CPU observation and scope exit, without inventing another
        // scheduler boundary or caching a numeric identity.
        assert!(unsafe { state.report_ready(0) });
    }
    assert!(!driver.advance());
    // SAFETY: the unpublished test object has no readers; the controlled period
    // is complete and execution occurs without bookkeeping locks held.
    assert_eq!(unsafe { state.invoke_ready(0, 1) }, 1);
    assert_eq!(calls.load(Ordering::Relaxed), 1);
}

#[unittest::def_test]
fn scheduler_boundary_before_period_does_not_satisfy_a_future_period() {
    let nodes = [ReaderNode::new()];
    let reader = TaskReader::new();
    // SAFETY: this isolated node has no other controller.
    let state = unsafe { CallbackState::new(&nodes, [true]) }.unwrap();
    let _driver = state.claim_driver().unwrap();
    let _guard = kspin::NoPreemptIrqSave::new();
    // SAFETY: stable isolated outgoing-task fixture under scheduler exclusion.
    assert!(!unsafe { state.note_context_switch(0, &reader) });
    let period = state.coordinator.begin_period().unwrap();
    // SAFETY: isolated watched fixture; rechecking cannot replace the missing
    // CPU observation for this newly initialized period.
    assert!(!unsafe { state.report_ready(0) });
    assert_eq!(state.coordinator.try_complete_period(), None);
    // SAFETY: another actual fixture boundary after initialization, not a cached
    // identity from the earlier call. The task is still inactive and retained.
    assert!(unsafe { state.note_context_switch(0, &reader) });
    assert_eq!(state.coordinator.try_complete_period(), Some(period));
}

#[unittest::def_test]
fn progress_hint_stops_after_cpu_report_even_with_a_blocked_reader() {
    let nodes = [ReaderNode::new(), ReaderNode::new()];
    let reader = TaskReader::new();
    // SAFETY: isolated stable nodes have one exclusive controller.
    let state = unsafe { CallbackState::new(&nodes, [true, false]) }.unwrap();
    let _driver = state.claim_driver().unwrap();
    assert!(!state.needs_cpu_observation(0));
    assert!(!state.needs_cpu_observation(1));
    assert!(!state.needs_cpu_observation(2));
    let observed = reader.with_read(|_| {
        let _guard = kspin::NoPreemptIrqSave::new();
        state.coordinator.begin_period().unwrap();
        let needed_before = state.needs_cpu_observation(0);
        // SAFETY: retained outgoing-task fixture, exclusively driven under
        // IRQ/preemption exclusion with no interrupt-context scope users.
        let reported = unsafe { state.note_context_switch(0, &reader) };
        (
            needed_before,
            reported,
            state.needs_cpu_observation(0),
            state.coordinator.try_complete_period().is_none(),
        )
    });
    assert_eq!(observed, (true, false, false, true));
    // SAFETY: watched fixture rechecks after its reader exits; no locks held.
    assert!(unsafe { state.report_ready(0) });
    assert!(state.coordinator.try_complete_period().is_some());
    assert!(!state.needs_cpu_observation(0));
}
