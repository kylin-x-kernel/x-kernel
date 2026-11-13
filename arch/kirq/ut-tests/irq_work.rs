// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Deterministic one-shot ownership and bounded-drain tests with a fake doorbell.

use alloc::vec::Vec;
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use kalloc_types::KBox;
use unittest::def_test;

use super::{CALLBACK_BUDGET, IrqWork, LocalState, QueueError, drain, enqueue, enqueue_inner};
use crate::context::test_support::{ScopedHardIrqContext, with_nmi_context};

static NOTIFICATIONS: AtomicUsize = AtomicUsize::new(0);
static CALLBACKS: AtomicUsize = AtomicUsize::new(0);

fn notify() {
    NOTIFICATIONS.fetch_add(1, Ordering::Relaxed);
}

fn prepared_local() -> LocalState {
    let mut local = LocalState::new();
    *local.notify.get_mut() = Some(notify);
    local.is_ready.store(true, Ordering::Release);
    local
}

/// # Safety
/// Invoke only as an ordinary IRQ callback while its node is live.
unsafe fn count_callback(_work: NonNull<IrqWork>) {
    assert!(!crate::context::is_in_nmi());
    assert!(crate::context::is_in_hardirq());
    CALLBACKS.fetch_add(1, Ordering::Relaxed);
}

/// # Safety
/// The pointer owns a KBox<IrqWork> detached from the queue exactly once.
unsafe fn free_callback(work: NonNull<IrqWork>) {
    // SAFETY: each test transfers exactly one KBox<IrqWork> through enqueue;
    // drain has already copied next and callback and will never touch it again.
    unsafe { drop(KBox::from_raw(work.as_ptr())) };
    CALLBACKS.fetch_add(1, Ordering::Relaxed);
}

#[def_test(serial)]
fn unavailable_queue_preserves_unclaimed_node() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let local = LocalState::new();
    let work = KBox::new_nowait(IrqWork::new(count_callback)).expect("IRQ work allocation failed");
    let pointer = NonNull::from(work.as_ref());
    // SAFETY: test owns the live stable KBox, and local IRQs are excluded.
    let result = unsafe { enqueue(&local, pointer) };
    assert_eq!(result, Err(QueueError::NotReady));
    assert!(!work.claimed.load(Ordering::Acquire));
    assert!(local.incoming.load(Ordering::Acquire).is_null());
}

#[def_test(serial)]
fn nested_nmi_publication_is_once_only_and_never_inline() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let local = prepared_local();
    CALLBACKS.store(0, Ordering::Relaxed);
    let first = KBox::new_nowait(IrqWork::new(count_callback)).expect("IRQ work allocation failed");
    let second =
        KBox::new_nowait(IrqWork::new(count_callback)).expect("IRQ work allocation failed");
    let publish_outer = || {
        // SAFETY: the pinned fixture retains the stable first KBox through drain.
        let result = unsafe { enqueue(&local, NonNull::from(first.as_ref())) };
        assert_eq!(result, Ok(()));
        let publish_inner = || {
            // SAFETY: the pinned fixture retains the stable second KBox through drain.
            let result = unsafe { enqueue(&local, NonNull::from(second.as_ref())) };
            assert_eq!(result, Ok(()));
        };
        // SAFETY: the outer pin excludes migration/IRQs throughout this balanced scope.
        unsafe { with_nmi_context(publish_inner) };
        // SAFETY: the fixture keeps the first KBox live for this duplicate submission.
        let result = unsafe { enqueue(&local, NonNull::from(first.as_ref())) };
        assert_eq!(result, Err(QueueError::AlreadyQueued));
        assert_eq!(CALLBACKS.load(Ordering::Relaxed), 0);
    };
    // SAFETY: the pin excludes migration/IRQs across both balanced synthetic NMI scopes.
    unsafe { with_nmi_context(publish_outer) };
    let _hardirq = ScopedHardIrqContext::enter();
    drain(&local);
    assert_eq!(CALLBACKS.load(Ordering::Relaxed), 2);
    // SAFETY: the KBox remains live after callback; one-shot reuse must fail.
    let result = unsafe { enqueue(&local, NonNull::from(first.as_ref())) };
    assert_eq!(result, Err(QueueError::AlreadyQueued));
    drain(&local);
    assert_eq!(CALLBACKS.load(Ordering::Relaxed), 2);
}

#[def_test(serial)]
fn bounded_batches_allow_callback_to_free_its_node() {
    let nodes: Vec<_> = (0..CALLBACK_BUDGET * 3 + 1)
        .map(|_| KBox::new_nowait(IrqWork::new(free_callback)).expect("IRQ work allocation failed"))
        .collect();
    let _pin = kspin::NoPreemptIrqSave::new();
    let local = prepared_local();
    CALLBACKS.store(0, Ordering::Relaxed);
    NOTIFICATIONS.store(0, Ordering::Relaxed);
    for node in nodes {
        let pointer = NonNull::new(KBox::into_raw(node)).unwrap();
        // SAFETY: stable KBox ownership transfers to the one-shot callback;
        // this test excludes ordinary IRQ consumption until explicit drain.
        let result = unsafe { enqueue(&local, pointer) };
        assert_eq!(result, Ok(()));
    }
    let _hardirq = ScopedHardIrqContext::enter();
    for batch in 1..=4 {
        drain(&local);
        assert_eq!(
            CALLBACKS.load(Ordering::Relaxed),
            (batch * CALLBACK_BUDGET).min(CALLBACK_BUDGET * 3 + 1)
        );
    }
    assert_eq!(
        NOTIFICATIONS.load(Ordering::Relaxed),
        CALLBACK_BUDGET * 3 + 1 + 3
    );
    assert!(local.incoming.load(Ordering::Acquire).is_null());
    drain(&local);
    assert_eq!(CALLBACKS.load(Ordering::Relaxed), CALLBACK_BUDGET * 3 + 1);
}

#[repr(C)]
struct ChainedNode {
    work: IrqWork,
    local: *const LocalState,
    child: NonNull<IrqWork>,
}

/// # Safety
/// The pointer owns the first field of a live ChainedNode with a live fixture
/// and a uniquely owned child allocation, in ordinary IRQ context.
unsafe fn chain_callback(work: NonNull<IrqWork>) {
    // SAFETY: repr(C) places work first; this callback owns the transferred KBox.
    let node = unsafe { KBox::from_raw(work.cast::<ChainedNode>().as_ptr()) };
    // SAFETY: the local fixture remains live and at its original address through drain.
    let local = unsafe { &*node.local };
    let publish_child = || {
        // SAFETY: the consumer is paused with IRQs/migration excluded, and the
        // preallocated child transfers exactly once to its freeing callback.
        let result = unsafe { enqueue(local, node.child) };
        assert_eq!(result, Ok(()));
    };
    // SAFETY: drain's fixture excludes IRQs/migration throughout this balanced scope.
    unsafe { with_nmi_context(publish_child) };
    CALLBACKS.fetch_add(1, Ordering::Relaxed);
}

#[def_test(serial)]
fn nmi_publication_during_callback_is_not_lost() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let local = prepared_local();
    CALLBACKS.store(0, Ordering::Relaxed);
    let child = NonNull::new(KBox::into_raw(
        KBox::new_nowait(IrqWork::new(free_callback)).expect("IRQ work allocation failed"),
    ))
    .unwrap();
    let parent = KBox::new_nowait(ChainedNode {
        work: IrqWork::new(chain_callback),
        local: &local,
        child,
    })
    .expect("chained IRQ work allocation failed");
    let pointer = NonNull::new(KBox::into_raw(parent))
        .unwrap()
        .cast::<IrqWork>();
    // SAFETY: both nodes have stable KBox storage and their fixture outlives drain.
    let result = unsafe { enqueue(&local, pointer) };
    assert_eq!(result, Ok(()));
    let _hardirq = ScopedHardIrqContext::enter();
    drain(&local);
    assert_eq!(CALLBACKS.load(Ordering::Relaxed), 2);
    assert!(local.incoming.load(Ordering::Acquire).is_null());
}

#[def_test(serial)]
fn nested_nmi_between_head_load_and_cas_preserves_both_nodes() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let local = prepared_local();
    CALLBACKS.store(0, Ordering::Relaxed);
    let outer = KBox::new_nowait(IrqWork::new(count_callback)).expect("IRQ work allocation failed");
    let inner = KBox::new_nowait(IrqWork::new(count_callback)).expect("IRQ work allocation failed");
    let publish_outer = || {
        let interrupt_publication = || {
            let publish_inner = || {
                // SAFETY: the pinned fixture retains the stable inner KBox through drain.
                let result = unsafe { enqueue(&local, NonNull::from(inner.as_ref())) };
                assert_eq!(result, Ok(()));
            };
            // SAFETY: the outer pin excludes migration/IRQs through this balanced scope.
            unsafe { with_nmi_context(publish_inner) };
        };
        let pointer = NonNull::from(outer.as_ref());
        // SAFETY: the pinned fixture retains both KBoxes through drain; the hook
        // only publishes the inner node before the outer CAS, without consuming either.
        let result = unsafe { enqueue_inner(&local, pointer, interrupt_publication) };
        assert_eq!(result, Ok(()));
    };
    // SAFETY: the pin excludes migration/IRQs across both balanced synthetic NMI scopes.
    unsafe { with_nmi_context(publish_outer) };
    let _hardirq = ScopedHardIrqContext::enter();
    drain(&local);
    assert_eq!(CALLBACKS.load(Ordering::Relaxed), 2);
}

#[def_test(serial)]
fn publication_at_budget_boundary_requests_another_interrupt() {
    let _pin = kspin::NoPreemptIrqSave::new();
    let local = prepared_local();
    CALLBACKS.store(0, Ordering::Relaxed);
    NOTIFICATIONS.store(0, Ordering::Relaxed);
    let child = NonNull::new(KBox::into_raw(
        KBox::new_nowait(IrqWork::new(free_callback)).expect("IRQ work allocation failed"),
    ))
    .unwrap();
    let parent = KBox::new_nowait(ChainedNode {
        work: IrqWork::new(chain_callback),
        local: &local,
        child,
    })
    .expect("chained IRQ work allocation failed");
    let pointer = NonNull::new(KBox::into_raw(parent))
        .unwrap()
        .cast::<IrqWork>();
    // SAFETY: live one-shot KBoxes transfer to callbacks with a stable fixture.
    let result = unsafe { enqueue(&local, pointer) };
    assert_eq!(result, Ok(()));
    for _ in 1..CALLBACK_BUDGET {
        let pointer = NonNull::new(KBox::into_raw(
            KBox::new_nowait(IrqWork::new(free_callback)).expect("IRQ work allocation failed"),
        ))
        .unwrap();
        // SAFETY: this callback uniquely owns its KBox after publication.
        let result = unsafe { enqueue(&local, pointer) };
        assert_eq!(result, Ok(()));
    }
    let _hardirq = ScopedHardIrqContext::enter();
    drain(&local);
    assert_eq!(CALLBACKS.load(Ordering::Relaxed), CALLBACK_BUDGET);
    assert_eq!(NOTIFICATIONS.load(Ordering::Relaxed), CALLBACK_BUDGET + 2);
    drain(&local);
    assert_eq!(CALLBACKS.load(Ordering::Relaxed), CALLBACK_BUDGET + 1);
}
