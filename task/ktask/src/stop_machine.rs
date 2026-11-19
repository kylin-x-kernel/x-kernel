// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU stop thread. A scheduler client, not nested in [`crate::run_queue`].
//!
//! Each CPU owns one pinned `migration/%u` task in [`ksched::KernelClass::Stop`].
//! It parks on a [`WaitQueue`] while idle and runs queued callbacks with IRQs
//! disabled. Callbacks must not sleep, block, or recurse into [`stop_one_cpu`].

use alloc::format;
use core::{
    cell::Cell,
    ptr::NonNull,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use khal::percpu::this_cpu_id;
use klazy::Once;
use kspin::NoPreemptIrqSave;

use crate::{KCpuMask, KtaskRef, TaskInner, TaskState, WaitQueue, activate_task, wake_task};

// `LazyInit::new()` is an uninitialized marker with no owned payload, so the
// per-CPU template copy does not duplicate a live stopper. The type path is
// resolved at this module, not inside the macro's generated wrapper.
kcpu_slot::cpu_slot_cell_with_template! {
    static CPU_STOPPER: lazyinit::LazyInit<crate::stop_machine::CpuStopper> =
        unsafe { lazyinit::LazyInit::new() };
}

struct StopWorkList {
    head: *mut CpuStopWork,
    tail: *mut CpuStopWork,
    len: usize,
}

/// Per-CPU stop-thread state.
pub(crate) struct CpuStopper {
    thread: KtaskRef,
    pending: kspin::SpinNoIrq<StopWorkList>,
    /// Queued-work count. Release updates in `enqueue_work` and `take_pending`
    /// publish list insertions and drains to the stop thread's Acquire load in
    /// `has_pending`, which is the park predicate.
    nr_pending: AtomicUsize,
    park: WaitQueue,
}

// SAFETY: the pending list pointers are read and written only under `pending`.
// `thread` and `park` are `Sync`. `nr_pending` is an atomic. Another CPU may
// enqueue, and this CPU's stopper drains; those two roles share this lock.
unsafe impl Send for CpuStopper {}
// SAFETY: see `Send`. Shared references are the cross-CPU enqueue path.
unsafe impl Sync for CpuStopper {}

struct CpuStopDone {
    /// Callback has finished. The Release store in [`CpuStopDone::complete`]
    /// publishes that callback's writes to the waiter, which Acquire-loads
    /// this flag in [`CpuStopDone::wait`].
    finished: AtomicBool,
    /// [`WaitQueue::notify_one`] has returned. event-listener wakes the waiter
    /// inside `Event::notify` and only then updates the event's heap inner, so
    /// the waiter must not drop this stack `WaitQueue` until it Acquire-loads
    /// the Release store in `complete`.
    notify_done: AtomicBool,
    wait: WaitQueue,
}

/// Caller-owned work item. Must stay at a fixed address from enqueue until the
/// callback returns. [`stop_one_cpu`] does not return until the stopper has
/// released this slot; [`stop_one_cpu_nowait`] callers must keep `work` pinned
/// until the callback has returned.
pub struct CpuStopWork {
    // The submitter may observe queued through &Self while the pending-list
    // owner updates this link; only the link needs interior mutability.
    next: Cell<*mut CpuStopWork>,
    callback: fn(*mut ()),
    arg: *mut (),
    done: *mut CpuStopDone,
    queued: AtomicBool,
}

// SAFETY: queueing is serialized by the target CPU's pending lock. The
// submitting CPU (or IRQ) owns the allocation until `queued` is cleared after
// the callback and before any `stop_one_cpu` waiter is woken. Watchdog keeps
// one per-CPU slot; `stop_one_cpu` keeps the work on the waiter's stack. The
// raw pointers are never dereferenced except by the stopper that drained
// them, or to link the list under that lock.
unsafe impl Send for CpuStopWork {}
// SAFETY: the pending lock (or detached consumer) exclusively mutates the
// Cell link. Callback metadata is immutable while queued; only the
// independent completion atomic is observed concurrently by the submitter.
unsafe impl Sync for CpuStopWork {}

impl CpuStopWork {
    /// Whether the stopper still owns this pinned slot.
    /// An Acquire observation of false after enqueue confirms the callback and
    /// all stopper accesses are finished; the caller may then reclaim storage.
    pub fn is_queued(&self) -> bool {
        self.queued.load(Ordering::Acquire)
    }

    /// Empty work slot, for per-CPU storage filled in before each nowait.
    pub const fn empty() -> Self {
        Self {
            next: Cell::new(core::ptr::null_mut()),
            callback: nop_stop_fn,
            arg: core::ptr::null_mut(),
            done: core::ptr::null_mut(),
            queued: AtomicBool::new(false),
        }
    }

    /// Build a work item that runs `callback(arg)` on the target CPU.
    pub const fn new(callback: fn(*mut ()), arg: *mut ()) -> Self {
        Self {
            next: Cell::new(core::ptr::null_mut()),
            callback,
            arg,
            done: core::ptr::null_mut(),
            queued: AtomicBool::new(false),
        }
    }
}

fn nop_stop_fn(_arg: *mut ()) {}

impl CpuStopDone {
    fn new() -> Self {
        Self {
            finished: AtomicBool::new(false),
            notify_done: AtomicBool::new(false),
            wait: WaitQueue::new(),
        }
    }

    fn complete(&self) {
        // Publishes the callback's writes to `wait`'s Acquire load of `finished`.
        self.finished.store(true, Ordering::Release);
        self.wait.notify_one(false);
        // Publishes the end of `notify_one`, including its later writes to this
        // `WaitQueue`'s heap inner, to the waiter's Acquire load of `notify_done`.
        self.notify_done.store(true, Ordering::Release);
    }

    fn wait(&self) {
        // Acquire observes the callback writes published by `complete`.
        self.wait
            .wait_until(|| self.finished.load(Ordering::Acquire));
        // `finished` is stored before `notify_one` returns. A remote waiter can
        // already be running here while the notifier still writes the event
        // heap. Spin: another wait on this same stack `Event` would recurse
        // into the object that must stay alive.
        while !self.notify_done.load(Ordering::Acquire) {
            core::hint::spin_loop();
        }
    }
}

impl CpuStopper {
    fn enqueue_work(&self, work: NonNull<CpuStopWork>, kick: bool) {
        let ptr = work.as_ptr();
        {
            let mut pending = self.pending.lock();
            // SAFETY: `work` is exclusive to this queue until the stopper
            // runs it; `next` is only written under `pending`.
            unsafe { work.as_ref() }.next.set(core::ptr::null_mut());
            if pending.tail.is_null() {
                pending.head = ptr;
            } else {
                // SAFETY: `tail` was linked in this list under the same lock.
                unsafe { (*pending.tail).next.set(ptr) };
            }
            pending.tail = ptr;
            pending.len += 1;
        }
        // Release publishes this insertion to `has_pending`'s Acquire load so
        // the stop thread rechecks the locked list instead of parking.
        self.nr_pending.fetch_add(1, Ordering::Release);
        // Do not `notify_one(true)`: that `yield_now()` requeues the caller on
        // this CPU before `stop_one_cpu` can `wait`. A local Fair waiter whose
        // new affinity forbids this CPU would resume here, see the stop already
        // complete, and never migrate. Wake the stopper; `kick` is for remote
        // / nowait. Local `stop_one_cpu` leaves via `wait` so pick can run Stop.
        self.park.notify_one(false);
        wake_task(&self.thread, kick);
    }

    fn take_pending(&self) -> *mut CpuStopWork {
        let mut pending = self.pending.lock();
        let head = pending.head;
        let taken = pending.len;
        pending.head = core::ptr::null_mut();
        pending.tail = core::ptr::null_mut();
        pending.len = 0;
        if taken > 0 {
            // Release publishes this drain to `has_pending`'s Acquire load,
            // the stop thread's park predicate.
            self.nr_pending.fetch_sub(taken, Ordering::Release);
        }
        head
    }

    fn has_pending(&self) -> bool {
        // Acquire observes the Release updates of `nr_pending`. Nonzero means
        // the locked list has work, so the stop thread must not park.
        self.nr_pending.load(Ordering::Acquire) != 0
    }
}

struct StopperRegistry([Once<usize>; kbuild_config::NR_CPUS]);

impl StopperRegistry {
    const fn new() -> Self {
        Self([const { Once::new() }; kbuild_config::NR_CPUS])
    }

    fn register(&self, cpu: LogicalCpuId, stopper: &CpuStopper) {
        self.0[cpu.as_usize()].call_once(|| core::ptr::from_ref(stopper).addr());
    }

    fn get(&self, cpu: LogicalCpuId) -> Option<&'static CpuStopper> {
        let addr = *self.0.get(cpu.as_usize())?.get()?;
        // SAFETY: each slot is written once during that CPU's stopper bring-up
        // and points at the per-CPU `CpuStopper` that lives for the kernel run.
        Some(unsafe { &*(addr as *const CpuStopper) })
    }
}

static STOPPERS: StopperRegistry = StopperRegistry::new();

/// This CPU's stopper.
///
/// Call only after [`init_current_cpu`] has installed this CPU's slot, from
/// that bring-up path or from this CPU's stop thread. There is no
/// current-process requirement; the slot is this CPU's shared stop-thread
/// state. Uses the caller's IRQ state, does not sleep, and requires no
/// caller-held lock.
fn current_stopper() -> &'static CpuStopper {
    CPU_STOPPER
        .shared_current()
        .get()
        .expect("stopper must be installed before the migration thread runs")
}

/// Spawn and activate this CPU's Stop-class helper.
///
/// Call after the timer IRQ handler is registered. Activating Stop can preempt
/// the caller immediately; `task_entry` then enables local IRQs, so a missing
/// timer descriptor would flood unhandled IRQs.
pub(crate) fn init_current_cpu() {
    let cpu_id = this_cpu_id();
    let task = TaskInner::new_internal(
        cpu_stopper_thread,
        format!("migration/{}", cpu_id.as_usize()),
        kbuild_config::TASK_STACK_SIZE,
    );
    // SAFETY: the pinned Stop thread owns the PM CPU rendezvous and runs only
    // IRQ-off callbacks; recovery does not acquire resources held by this task.
    let thread =
        unsafe { task.with_pm_infrastructure() }.into_arc_kernel(ksched::KernelClass::Stop);
    thread.set_cpumask(KCpuMask::one_shot_logical(cpu_id));
    #[cfg(feature = "smp")]
    thread.set_cpu_id(cpu_id);
    thread.set_state(TaskState::Ready);
    #[cfg(feature = "snapshot")]
    crate::task_registry::record_tracked_task(&thread);

    let stopper = CpuStopper {
        thread: thread.clone(),
        pending: kspin::SpinNoIrq::new(StopWorkList {
            head: core::ptr::null_mut(),
            tail: core::ptr::null_mut(),
            len: 0,
        }),
        nr_pending: AtomicUsize::new(0),
        park: WaitQueue::new(),
    };
    {
        let guard = NoPreemptIrqSave::new();
        // SAFETY: the guard pins this CPU. Bring-up publishes this slot once,
        // before any queue, and the storage lasts for the kernel run.
        let slot = unsafe { CPU_STOPPER.get_mut_unchecked(&guard) };
        slot.init_once(stopper);
    }
    STOPPERS.register(cpu_id, current_stopper());
    activate_task(&thread);
}

fn cpu_stopper_thread() {
    loop {
        karch::disable_local_irq();
        let head = current_stopper().take_pending();
        run_taken_stop_work(head);
        karch::enable_local_irq();
        let stopper = current_stopper();
        stopper.park.wait_until(|| stopper.has_pending());
    }
}

/// Queue `callback(arg)` on `cpu` and wait until it has run.
///
/// Must be called from sleepable task context. Returns `false` if `cpu` has
/// no stopper yet, or if this work slot is already queued. Run-queue
/// registration can finish before the per-CPU stopper is installed, so a
/// successful scheduler init does not mean this call will succeed. The
/// stacked work item stays live until this call returns, including when
/// `cpu` is remote. The waiter does not drop it when `finished` becomes
/// visible: `notify_one` wakes that task before it finishes writing the
/// stack `Event`'s heap inner, and `wait` spins until that write is done.
///
/// # Panics
///
/// Panics in hardirq, softirq, or BH-disabled context.
///
/// # Examples
///
/// After [`crate::init_percpu_helpers`] has installed `migration/%u`, a
/// sleepable caller can park until the Stop thread has run `callback`:
///
/// ```rust,ignore
/// use khal::percpu::this_cpu_id;
/// use ktask::stop_one_cpu;
///
/// fn on_cpu(_arg: *mut ()) {
///     // IRQ-off, no sleep, no nested stop.
/// }
///
/// assert!(stop_one_cpu(this_cpu_id(), on_cpu, core::ptr::null_mut()));
/// ```
pub fn stop_one_cpu(cpu: LogicalCpuId, callback: fn(*mut ()), arg: *mut ()) -> bool {
    crate::api::assert_sleepable_context("stop_one_cpu");
    let mut done = CpuStopDone::new();
    let mut work = CpuStopWork::new(callback, arg);
    work.done = &mut done as *mut CpuStopDone;
    // Local: do not resched the caller before `wait`, or Stop can finish while
    // the waiter is still Running on a now-forbidden CPU. Remote: kick the
    // target so its stopper runs while this task sleeps.
    if !queue_on_cpu(cpu, &mut work, cpu != this_cpu_id()) {
        return false;
    }
    done.wait();
    true
}

/// Queue `work` on `cpu` without waiting. Safe from IRQ context.
///
/// `work` must remain pinned until the callback returns. Returns `false` if
/// the slot is already queued, or if `cpu` has no stopper yet (same bring-up
/// window as [`stop_one_cpu`]).
///
/// # Examples
///
/// The timer IRQ reuses a per-CPU [`CpuStopWork`] so the Stop thread can
/// touch watchdog state without waiting in IRQ context:
///
/// ```rust,ignore
/// use khal::percpu::this_cpu_id;
/// use ktask::{CpuStopWork, stop_one_cpu_nowait};
///
/// fn touch(_arg: *mut ()) {}
///
/// let mut work = CpuStopWork::empty();
/// let _queued = stop_one_cpu_nowait(
///     this_cpu_id(),
///     touch,
///     core::ptr::null_mut(),
///     &mut work,
/// );
/// // Keep `work` pinned until `touch` returns.
/// ```
pub fn stop_one_cpu_nowait(
    cpu: LogicalCpuId,
    callback: fn(*mut ()),
    arg: *mut (),
    work: &mut CpuStopWork,
) -> bool {
    // Claim the slot before writing it. A failed claim means a stopper may
    // already be reading these fields on another CPU.
    if !claim_work(work) {
        return false;
    }
    work.callback = callback;
    work.arg = arg;
    work.done = core::ptr::null_mut();
    enqueue_claimed(cpu, work, true)
}

fn queue_on_cpu(cpu: LogicalCpuId, work: &mut CpuStopWork, kick: bool) -> bool {
    if !claim_work(work) {
        return false;
    }
    enqueue_claimed(cpu, work, kick)
}

/// AcqRel claims the slot. A failed Acquire observes the caller that already
/// queued it; that caller must not write the item.
fn claim_work(work: &CpuStopWork) -> bool {
    work.queued
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

fn enqueue_claimed(cpu: LogicalCpuId, work: &mut CpuStopWork, kick: bool) -> bool {
    let Some(stopper) = STOPPERS.get(cpu) else {
        // Nothing queued this slot. Release lets a later claim observe it free.
        work.queued.store(false, Ordering::Release);
        return false;
    };
    stopper.enqueue_work(NonNull::from(work), kick);
    true
}

fn run_taken_stop_work(mut cursor: *mut CpuStopWork) {
    while !cursor.is_null() {
        // SAFETY: the detached chain owns this pinned slot until the Release
        // store below. The submitter can concurrently read the completion flag.
        let work = unsafe { &*cursor };
        cursor = work.next.replace(core::ptr::null_mut());
        let callback = work.callback;
        let arg = work.arg;
        let done = work.done;
        callback(arg);
        // Release publishes "the callback has finished and the slot is free"
        // to the next AcqRel claim of `queued`. `stop_one_cpu` waiters ignore
        // this flag and keep the stack frame until `notify_done`.
        // This is the final access to the slot, permitting immediate reclaim.
        work.queued.store(false, Ordering::Release);
        if !done.is_null() {
            // SAFETY: `stop_one_cpu` keeps `CpuStopDone` on the caller's stack
            // until `wait` observes `notify_done`. That store is the last
            // access in `complete`, after `notify_one` has finished writing
            // the stack `Event`.
            unsafe {
                (*done).complete();
            }
        }
    }
}
