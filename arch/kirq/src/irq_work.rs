// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One-shot, allocation-free local handoff from NMI to ordinary hard IRQ.
//!
//! Nodes are prepared before submission. A successful submission transfers the
//! callback responsibility, not a borrow of caller-owned temporary storage.
//! Callbacks cannot sleep. Use a normal worker for sleepable finalization.
//! There is no cancellation, flush, remote submission, or CPU-offline protocol.

use core::{
    cell::UnsafeCell,
    marker::PhantomPinned,
    ptr::{self, NonNull},
    sync::atomic::{AtomicBool, AtomicPtr, Ordering},
};

use kcpu_slot::{StaticSlotValue, cpu_slot};
use kspin::NoPreemptIrqSave;

use crate::{context, platform::IntrManagerIf};

/// Maximum callbacks executed by one ordinary interrupt invocation.
pub const CALLBACK_BUDGET: usize = 64;

/// A stable-address node submitted at most once during its lifetime.
///
/// No ownership is hidden in the node. Its callback owns the protocol for
/// releasing or forwarding the containing allocation after unlinking.
pub struct IrqWork {
    next: UnsafeCell<*mut IrqWork>,
    callback: unsafe fn(NonNull<IrqWork>),
    claimed: AtomicBool,
    _pin: PhantomPinned,
}

// SAFETY: callers transfer a stable live node under queue's lifetime contract.
// Only its successful producer writes next before release publication; the
// local consumer reads it after acquire and never accesses it after callback.
unsafe impl Send for IrqWork {}
// SAFETY: claimed serializes producers. Callback and pin marker are immutable;
// next has the single-producer/publication/consumer protocol described above.
unsafe impl Sync for IrqWork {}

impl IrqWork {
    /// Prepares a one-shot node without allocating or touching CPU state.
    ///
    /// The callback runs in ordinary hardirq context with local IRQs disabled.
    /// It must preserve that context, never sleep, and never wait for work on
    /// this CPU. It may free its node: the dispatcher stops accessing it first.
    pub const fn new(callback: unsafe fn(NonNull<Self>)) -> Self {
        Self {
            next: UnsafeCell::new(ptr::null_mut()),
            callback,
            claimed: AtomicBool::new(false),
            _pin: PhantomPinned,
        }
    }
}

/// Submission failed without publishing a new callback responsibility.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueError {
    /// This CPU has no initialized NMI-safe notification provider.
    NotReady,
    /// This node was already submitted; it cannot be reused after execution.
    AlreadyQueued,
}

struct LocalState {
    incoming: AtomicPtr<IrqWork>,
    backlog: UnsafeCell<*mut IrqWork>,
    notify: UnsafeCell<Option<fn()>>,
    is_ready: AtomicBool,
    is_running: AtomicBool,
}

impl LocalState {
    const fn new() -> Self {
        Self {
            incoming: AtomicPtr::new(ptr::null_mut()),
            backlog: UnsafeCell::new(ptr::null_mut()),
            notify: UnsafeCell::new(None),
            is_ready: AtomicBool::new(false),
            is_running: AtomicBool::new(false),
        }
    }

    fn notification(&self) -> Option<fn()> {
        if !self.is_ready.load(Ordering::Acquire) {
            return None;
        }
        // SAFETY: init_current_cpu writes once before publishing is_ready;
        // no teardown or replacement can mutate this slot afterwards.
        unsafe { *self.notify.get() }
    }
}

// SAFETY: the static template contains only null pointers, false atomics and
// None, with no ownership or self references. Each CPU initializes its copy.
unsafe impl StaticSlotValue for LocalState {}
// SAFETY: incoming and readiness are atomic; notify is immutable after release
// publication. Only the IRQ-off, is_running-protected local consumer uses backlog.
unsafe impl Sync for LocalState {}

cpu_slot! {
    static LOCAL: crate::irq_work::LocalState = crate::irq_work::LocalState::new();
}

/// Installs this CPU's permanent ordinary-IRQ notification capability.
///
/// Call during task/boot setup after CPU slots, allocator, trap and controller
/// initialization, before exposing NMI users. Returns false on unsupported
/// platforms or in interrupt context. Repeated successful initialization is a
/// no-op. This neither starts workers nor executes callbacks.
pub fn init_current_cpu() -> bool {
    if context::is_in_interrupt_context() {
        return false;
    }
    let _guard = NoPreemptIrqSave::new();
    let local = LOCAL.current_shared();
    if local.is_ready.load(Ordering::Acquire) {
        return true;
    }
    let Some(notify) = IntrManagerIf::prepare_local_irq_work() else {
        return false;
    };
    // SAFETY: local IRQ/preemption exclusion serializes initialization on this
    // CPU. NMI readers return NotReady until the release publication below.
    unsafe { *local.notify.get() = Some(notify) };
    local.is_ready.store(true, Ordering::Release);
    true
}

/// Reports readiness on this CPU without taking a lock, including from NMI.
/// Requires initialized CPU slots; the result is not a transferable CPU token.
pub fn is_ready() -> bool {
    LOCAL.current_shared().is_ready.load(Ordering::Acquire)
}

/// Transfers a prepared one-shot node to this CPU's ordinary IRQ consumer.
///
/// Supports task, softirq, hardirq and NMI after local initialization. NMI
/// submission uses only atomics and the certified notification function. Other
/// contexts mask local IRQs and pin execution across publication. CAS may retry;
/// this is not a wait-free or bounded-latency interface. IRQ masking or a stopped
/// CPU can postpone execution indefinitely. No FIFO ordering is promised.
///
/// # Safety
/// CPU slots must be initialized. The node must be fully initialized at a stable
/// address and remain live through queue access and callback handoff. On success
/// callers relinquish access that could move, modify or free it; the callback
/// owns its eventual cleanup. Concurrent duplicate submissions require separate
/// external lifetime protection for every submitter. The callback must obey
/// new's hardirq contract, and its containing storage must be safe to transfer
/// to the submitting CPU. Never submit stack storage that expires before use.
///
/// # Errors
/// NotReady does not claim or publish the node. AlreadyQueued does not cancel or extend
/// the earlier submission's lifetime. Neither error executes the callback.
pub unsafe fn queue(work: NonNull<IrqWork>) -> Result<(), QueueError> {
    let _guard = if context::is_in_nmi() {
        None
    } else {
        Some(NoPreemptIrqSave::new())
    };
    let local = LOCAL.current_shared();
    // SAFETY: caller supplies the node lifetime; IRQ exclusion or NMI execution
    // prevents migration and ordinary local consumption during publication.
    unsafe { enqueue(local, work) }
}

/// Publishes into one CPU queue with local consumption excluded.
///
/// # Safety
/// The caller must meet queue's node contract and exclude ordinary local
/// consumption and migration until publication and notification finish.
unsafe fn enqueue(local: &LocalState, work: NonNull<IrqWork>) -> Result<(), QueueError> {
    // SAFETY: forwards the same node lifetime and local consumption exclusion.
    unsafe {
        enqueue_inner(
            local,
            work,
            #[cfg(unittest)]
            || {},
        )
    }
}

/// Publishes with an optional test-only interruption before the first CAS.
///
/// # Safety
/// The node and local execution must meet enqueue's contract.
unsafe fn enqueue_inner(
    local: &LocalState,
    work: NonNull<IrqWork>,
    #[cfg(unittest)] before_publish: impl FnOnce(),
) -> Result<(), QueueError> {
    let notify = local.notification().ok_or(QueueError::NotReady)?;
    // SAFETY: caller supplies live, initialized, stable storage through handoff.
    let node = unsafe { work.as_ref() };
    node.claimed
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Relaxed)
        .map_err(|_| QueueError::AlreadyQueued)?;
    let mut head = local.incoming.load(Ordering::Relaxed);
    #[cfg(unittest)]
    before_publish();
    loop {
        // SAFETY: only the winning one-shot producer accesses this unpublished
        // link; the consumer cannot see it before the release CAS succeeds.
        unsafe { *node.next.get() = head };
        match local.incoming.compare_exchange_weak(
            head,
            work.as_ptr(),
            Ordering::Release,
            Ordering::Relaxed,
        ) {
            Ok(_) => break,
            Err(observed) => head = observed,
        }
    }
    notify();
    Ok(())
}

/// Drains a bounded batch from the platform's ordinary IRQ handler.
///
/// Calls outside ordinary hardirq context are ignored, including NMI exits.
/// The platform must only wire this to an ordinary-priority interrupt. Callbacks
/// run IRQ-off and may free their own nodes; no node is touched after dispatch.
pub fn handle_irq() {
    if context::is_in_nmi() || !context::is_in_hardirq() {
        return;
    }
    let _guard = NoPreemptIrqSave::new();
    let local = LOCAL.current_shared();
    drain(local);
}

fn drain(local: &LocalState) {
    let Some(notify) = local.notification() else {
        return;
    };
    if local.is_running.swap(true, Ordering::Acquire) {
        return;
    }
    for _ in 0..CALLBACK_BUDGET {
        // SAFETY: only this CPU's non-reentrant ordinary IRQ consumer accesses
        // backlog. NMI producers modify incoming, never this detached batch.
        let mut head = unsafe { *local.backlog.get() };
        if head.is_null() {
            head = local.incoming.swap(ptr::null_mut(), Ordering::Acquire);
        }
        let Some(work) = NonNull::new(head) else {
            break;
        };
        // SAFETY: acquire-detach owns a live published node. Copy everything
        // before invoking its callback, which may release the allocation.
        let node = unsafe { work.as_ref() };
        // SAFETY: the acquired node link is now exclusively consumer-owned.
        let next = unsafe { *node.next.get() };
        let callback = node.callback;
        // SAFETY: this CPU's IRQ-off consumer exclusively owns backlog.
        unsafe { *local.backlog.get() = next };
        // SAFETY: queue's caller certified the one-shot callback/storage
        // contract; this is ordinary hardirq, IRQ-off, after unlinking the node.
        unsafe { callback(work) };
    }
    // SAFETY: the same non-reentrant local consumer still owns backlog.
    let has_backlog = unsafe { !(*local.backlog.get()).is_null() };
    local.is_running.store(false, Ordering::Release);
    if has_backlog || !local.incoming.load(Ordering::Acquire).is_null() {
        notify();
    }
}

unittest::path_mod_test!(tests, "../ut-tests/irq_work.rs");
