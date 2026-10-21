// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot service state persists for the test run; all accepted probes finish
//! before their observers are released. No global action or service is replaced.

use alloc::{boxed::Box, sync::Arc};
use core::{sync::atomic::{AtomicBool, AtomicUsize, Ordering}, task::Waker};

use ktask::future::PreparedTaskWait;
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::{CallbackNode, PreparedCallback};

struct Counts {
    invoked: AtomicUsize,
    cancelled: AtomicUsize,
    dropped: AtomicUsize,
    completed: AtomicUsize,
    has_invalid_context: AtomicBool,
    has_repost_failure: AtomicBool,
    has_submission_allocation: AtomicBool,
}

impl Counts {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            invoked: AtomicUsize::new(0),
            cancelled: AtomicUsize::new(0),
            dropped: AtomicUsize::new(0),
            completed: AtomicUsize::new(0),
            has_invalid_context: AtomicBool::new(false),
            has_repost_failure: AtomicBool::new(false),
            has_submission_allocation: AtomicBool::new(false),
        })
    }
}

#[repr(C, align(128))]
struct Probe {
    // Deliberately put the node at a nonzero offset in an over-aligned object.
    value: usize,
    reposts_left: usize,
    node: CallbackNode,
    counts: Arc<Counts>,
    waker: Waker,
}

impl Drop for Probe {
    fn drop(&mut self) {
        self.counts.dropped.fetch_add(1, Ordering::Relaxed);
    }
}

/// # Safety
/// `node` is the unique raw projection produced by prepare_probe, still owned by
/// either cancellation or detached invocation. Recover the Box exactly once.
unsafe fn recover_probe(node: *mut CallbackNode) -> Box<Probe> {
    // SAFETY: prepare_probe projected from the full allocation pointer, retaining
    // provenance. Subtracting the actual field offset restores its aligned base.
    let object = unsafe { node.byte_sub(core::mem::offset_of!(Probe, node)) }.cast::<Probe>();
    // SAFETY: the unique raw Box owner was transferred into this handle; no
    // references or competing invocation survive at this recovery point.
    unsafe { Box::from_raw(object) }
}

/// # Safety
/// Receives the sole unsubmitted owner established by prepare_probe. The object
/// was never published to readers, so cancellation needs no grace period.
unsafe fn cancel_probe(node: *mut CallbackNode) {
    // SAFETY: the handle invokes cancellation only before successful transfer.
    let object = unsafe { recover_probe(node) };
    object.counts.cancelled.fetch_add(1, Ordering::Relaxed);
}

/// # Safety
/// Receives prepare_probe's unique detached owner after its required GP. Queue
/// bookkeeping holds no lock and will not access the node after this invocation.
unsafe fn complete_probe(node: *mut CallbackNode) {
    // SAFETY: accepted submission transferred this original Box to invocation.
    let mut object = unsafe { recover_probe(node) };
    let counts = object.counts.clone();
    let waker = object.waker.clone();
    let is_valid_context = kirq::context::interrupt_context_level()
        == kirq::context::InterruptContextLevel::Softirq
        && krcu_core::RcuContextIf::current_cpu_watching().is_watching()
        && !ktask::is_sleepable_context();
    if !is_valid_context {
        counts.has_invalid_context.store(true, Ordering::Release);
    }
    counts.invoked.fetch_add(object.value, Ordering::Relaxed);
    if is_valid_context && object.reposts_left != 0 {
        object.reposts_left -= 1;
        let prepared = adopt_probe(object);
        let guard = kspin::NoPreemptIrqSave::new();
        let allocated_before = kalloc::current_cpu_allocated_bytes_for_test(&guard);
        // SAFETY: the current invocation exclusively owns this detached,
        // unpublished node. Reposting transfers the same allocation, without
        // allocating, waiting or holding a queue lock. No node access follows.
        let submitted = unsafe { prepared.submit() };
        let allocated_after = kalloc::current_cpu_allocated_bytes_for_test(&guard);
        drop(guard);
        if allocated_after != allocated_before {
            counts.has_submission_allocation.store(true, Ordering::Release);
        }
        match submitted {
            Ok(()) => return,
            Err((_, prepared)) => {
                counts.has_repost_failure.store(true, Ordering::Release);
                drop(prepared);
            }
        }
    } else {
        drop(object);
    }
    // Publish only after releasing the enclosing allocation, so the observer
    // can check destructor accounting without racing the callback's own drop.
    counts.completed.store(1, Ordering::Release);
    waker.wake();
}

fn prepare_probe(counts: &Arc<Counts>, waker: Waker, reposts: usize) -> PreparedCallback {
    adopt_probe(Box::new(Probe {
        value: 7,
        reposts_left: reposts,
        node: CallbackNode::new(complete_probe),
        counts: counts.clone(),
        waker,
    }))
}

fn adopt_probe(object: Box<Probe>) -> PreparedCallback {
    let object = Box::into_raw(object);
    // SAFETY: raw field projection retains the full original Box provenance.
    let node = unsafe { &raw mut (*object).node };
    // SAFETY: a field of a live Box cannot have a null address.
    let node = unsafe { NonNull::new_unchecked(node) };
    // SAFETY: unpublished Box storage is fixed and exclusively transferred.
    // Both cleanup paths recover its actual field offset and release it once.
    // Its Arc counters/waker are transferable and permit nonblocking cleanup.
    unsafe { PreparedCallback::new(node, cancel_probe) }
}

#[def_test(serial)]
fn public_callback_submission_and_drain_release_owner_once() {
    let counts = Counts::new();
    let wait = PreparedTaskWait::prepare().unwrap();
    let prepared = prepare_probe(&counts, wait.waker(), 0);
    // SAFETY: this fresh probe was never published, owns transferable storage
    // and has nonblocking cleanup. No scheduler or RCU lock is held.
    let submitted = unsafe { prepared.submit() };
    assert!(submitted.is_ok());
    // SAFETY: no producer remains, callback code is permanent and counts/wait
    // storage remains live. This task holds no resource needed by the callback.
    assert_eq!(unsafe { crate::drain_callbacks() }, Ok(()));
    assert_eq!(counts.completed.load(Ordering::Acquire), 1);
    assert_eq!(counts.invoked.load(Ordering::Relaxed), 7);
    assert_eq!(counts.cancelled.load(Ordering::Relaxed), 0);
    assert_eq!(counts.dropped.load(Ordering::Relaxed), 1);
    assert!(!counts.has_invalid_context.load(Ordering::Acquire));
}

#[def_test]
fn unsubmitted_handle_releases_preparation_without_invocation() {
    let counts = Counts::new();
    let wait = PreparedTaskWait::prepare().unwrap();
    let prepared = prepare_probe(&counts, wait.waker(), 0);
    drop(prepared);
    assert_eq!(counts.cancelled.load(Ordering::Relaxed), 1);
    assert_eq!(counts.dropped.load(Ordering::Relaxed), 1);
    assert_eq!(counts.invoked.load(Ordering::Relaxed), 0);
    assert_eq!(counts.completed.load(Ordering::Acquire), 0);
}

#[def_test(serial)]
fn rejected_contexts_preserve_same_owner_until_watched_retry() {
    let counts = Counts::new();
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let prepared = prepare_probe(&counts, wait.waker(), 0);
    let original_node = prepared.node;
    let result = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let submit_fn = || {
            // SAFETY: this unpublished probe has no protected readers; its
            // cancellation/invocation are nonblocking. NMI must reject it.
            unsafe { prepared.submit() }
        };
        // SAFETY: pinned, IRQ-masked, balanced controlled NMI with no blocking
        // work. Submission must reject before acquiring any ordinary lock.
        unsafe { kirq::context::test_support::with_nmi_context(submit_fn) }
    };
    let (error, prepared) = match result {
        Err(rejected) => rejected,
        Ok(()) => panic!("NMI callback submission was accepted"),
    };
    assert_eq!(error, KError::InvalidInput);
    assert_eq!(prepared.node, original_node);
    assert_eq!(counts.cancelled.load(Ordering::Relaxed), 0);
    assert_eq!(counts.dropped.load(Ordering::Relaxed), 0);
    let result = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let watching = krcu_core::RcuContextIf::current_cpu_watching();
        // SAFETY: pinned CPU, IRQs excluded, no active reader. The controlled
        // window contains only rejection; no callback or destructor may run.
        unsafe { watching.enter_quiescent() };
        // SAFETY: unchanged unique unpublished node. Unwatched admission must
        // return it before touching any queue or requesting notification.
        let result = unsafe { prepared.submit() };
        // SAFETY: same CPU and exclusion, paired before any assertion or drop.
        unsafe { watching.exit_quiescent() };
        result
    };
    let (error, prepared) = match result {
        Err(rejected) => rejected,
        Ok(()) => panic!("unwatched callback submission was accepted"),
    };
    assert_eq!(error, KError::InvalidInput);
    assert_eq!(prepared.node, original_node);
    assert_eq!(counts.invoked.load(Ordering::Relaxed), 0);
    assert_eq!(counts.cancelled.load(Ordering::Relaxed), 0);
    assert_eq!(counts.dropped.load(Ordering::Relaxed), 0);
    // SAFETY: unchanged unpublished owner, now in watched task context with
    // no locks. Both storage recovery and cleanup are softirq-safe.
    let accepted = unsafe { prepared.submit() };
    assert!(accepted.is_ok());
    while counts.completed.load(Ordering::Acquire) == 0 {
        wait.park();
    }
    assert_eq!(counts.invoked.load(Ordering::Relaxed), 7);
    assert_eq!(counts.cancelled.load(Ordering::Relaxed), 0);
    assert_eq!(counts.dropped.load(Ordering::Relaxed), 1);
    assert!(!counts.has_invalid_context.load(Ordering::Acquire));
}

#[def_test(serial)]
fn softirq_reposts_the_same_embedded_node_without_reallocating() {
    let counts = Counts::new();
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let prepared = prepare_probe(&counts, wait.waker(), 3);
    // SAFETY: unpublished probe, watched task context, no held locks. Each
    // nonblocking callback adopts the same allocation and submits it once.
    let accepted = unsafe { prepared.submit() };
    assert!(accepted.is_ok());
    while counts.completed.load(Ordering::Acquire) == 0 {
        wait.park();
    }
    assert_eq!(counts.invoked.load(Ordering::Relaxed), 4 * 7);
    assert_eq!(counts.cancelled.load(Ordering::Relaxed), 0);
    assert_eq!(counts.dropped.load(Ordering::Relaxed), 1);
    assert!(!counts.has_invalid_context.load(Ordering::Acquire));
    assert!(!counts.has_repost_failure.load(Ordering::Acquire));
    assert!(!counts.has_submission_allocation.load(Ordering::Acquire));
}

#[def_test(serial)]
fn timer_hardirq_submits_prepared_callback_to_live_service() {
    use ktime_types::TimeSpan;

    // This live integration fixture uses timer delivery only to enter actual
    // hardirq. No elapsed-time assertion or performance requirement is tested;
    // the outer QEMU timeout diagnoses a stalled delivery/service.
    let counts = Counts::new();
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let prepared = prepare_probe(&counts, wait.waker(), 0);
    let pending = Arc::new(kspin::SpinNoIrq::new(Some(prepared)));
    let attempted = Arc::new(AtomicBool::new(false));
    let accepted = Arc::new(AtomicBool::new(false));
    let irq_pending = pending.clone();
    let irq_attempted = attempted.clone();
    let irq_accepted = accepted.clone();
    let irq_counts = counts.clone();
    let irq_waker = wait.waker();
    let timer = ktask::Timer::new(move |_: ktask::TimerControl<'_>, _: ktask::Expiration| {
        let is_valid_context = kirq::context::interrupt_context_level()
            == kirq::context::InterruptContextLevel::Hardirq
            && krcu_core::RcuContextIf::current_cpu_watching().is_watching()
            && !karch::local_irq_enabled();
        if is_valid_context {
            // Release the fixture lock before submission can request progress.
            let prepared = irq_pending.lock().take();
            if let Some(prepared) = prepared {
                let guard = kspin::NoPreemptIrqSave::new();
                let allocated_before = kalloc::current_cpu_allocated_bytes_for_test(&guard);
                // SAFETY: all storage/waker resources were prepared in task
                // context. The probe is unpublished and its callback and
                // cancellation are nonblocking; no fixture/RCU lock is held.
                let submitted = unsafe { prepared.submit() };
                let allocated_after = kalloc::current_cpu_allocated_bytes_for_test(&guard);
                drop(guard);
                if allocated_after != allocated_before {
                    irq_counts.has_submission_allocation.store(true, Ordering::Release);
                }
                match submitted {
                    Ok(()) => irq_accepted.store(true, Ordering::Release),
                    Err((_, prepared)) => *irq_pending.lock() = Some(prepared),
                }
            }
        } else {
            irq_counts.has_invalid_context.store(true, Ordering::Release);
        }
        irq_attempted.store(true, Ordering::Release);
        irq_waker.wake_by_ref();
    });
    // SAFETY: boot initialized timer service. The one-shot handler does not
    // allocate, block, rearm or retain any lock across submission/notification.
    let started = unsafe { timer.start(khal::time::monotonic_time() + TimeSpan::from_millis(1)) };
    if started.is_ok() {
        while !attempted.load(Ordering::Acquire) {
            wait.park();
        }
    }
    timer.cancel_sync();
    drop(timer);
    let was_accepted = accepted.load(Ordering::Acquire);
    if was_accepted {
        while counts.completed.load(Ordering::Acquire) == 0 {
            wait.park();
        }
    }
    // Rejected or unattempted storage is released in task context, outside the
    // fixture lock, after the timer can no longer touch it.
    let rejected = pending.lock().take();
    drop(rejected);
    assert!(started.is_ok() && was_accepted);
    assert!(!counts.has_invalid_context.load(Ordering::Acquire));
    assert!(!counts.has_submission_allocation.load(Ordering::Acquire));
    assert_eq!(counts.invoked.load(Ordering::Relaxed), 7);
    assert_eq!(counts.cancelled.load(Ordering::Relaxed), 0);
    assert_eq!(counts.dropped.load(Ordering::Relaxed), 1);
}

#[def_test(serial)]
fn coalesced_notifications_drain_prepared_callbacks_across_budgets() {
    let mut wait = PreparedTaskWait::prepare().unwrap();
    let count = super::super::CALLBACK_BUDGET * 2 + 1;
    let observers: alloc::vec::Vec<_> = (0..count).map(|_| Counts::new()).collect();
    let prepared: alloc::vec::Vec<_> = observers
        .iter()
        .map(|counts| prepare_probe(counts, wait.waker(), 0))
        .collect();
    let mut accepted = 0;
    let has_completed_before_release;
    let has_submission_allocation;
    {
        // All submissions select this CPU. Its executor cannot run until this
        // exclusion ends, so notifications accumulate before any callback runs.
        // No fixture or RCU lock spans submission; all storage already exists.
        let guard = kspin::NoPreemptIrqSave::new();
        let allocated_before = kalloc::current_cpu_allocated_bytes_for_test(&guard);
        for callback in prepared {
            // SAFETY: each handle uniquely owns an unpublished probe. Both
            // cancellation and invocation are nonblocking and retain observers.
            match unsafe { callback.submit() } {
                Ok(()) => accepted += 1,
                Err((_, callback)) => drop(callback),
            }
        }
        has_submission_allocation =
            kalloc::current_cpu_allocated_bytes_for_test(&guard) != allocated_before;
        has_completed_before_release = observers
            .iter()
            .any(|counts| counts.completed.load(Ordering::Acquire) != 0);
    }
    // Rejection is also terminal for fixture cleanup. Do not strand accepted
    // callbacks when reporting a failed admission assertion.
    while observers.iter().any(|counts| {
        counts.completed.load(Ordering::Acquire) == 0
            && counts.cancelled.load(Ordering::Relaxed) == 0
    }) {
        wait.park();
    }
    assert!(!has_completed_before_release);
    assert!(!has_submission_allocation);
    assert_eq!(accepted, count);
    for counts in observers {
        assert_eq!(counts.invoked.load(Ordering::Relaxed), 7);
        assert_eq!(counts.cancelled.load(Ordering::Relaxed), 0);
        assert_eq!(counts.dropped.load(Ordering::Relaxed), 1);
        assert!(!counts.has_invalid_context.load(Ordering::Acquire));
    }
}
