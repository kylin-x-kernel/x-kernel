// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Fault injection into the current shared executor with a mocked queue.
//! Host unwind is a test observation tool, not kernel panic recovery.

#![feature(allocator_api)]

extern crate alloc;
extern crate self as kirq;

struct CheckedAllocator;

#[global_allocator]
static ALLOCATOR: CheckedAllocator = CheckedAllocator;

// SAFETY: this wrapper preserves System's allocation/layout contract and only
// checks a preinitialized thread-local flag before forwarding each operation.
unsafe impl std::alloc::GlobalAlloc for CheckedAllocator {
    unsafe fn alloc(&self, layout: std::alloc::Layout) -> *mut u8 {
        assert!(
            !context::IN_NMI
                .try_with(|state| state.get())
                .unwrap_or(false)
        );
        // SAFETY: forwards the caller's valid allocation layout unchanged.
        unsafe { std::alloc::System.alloc(layout) }
    }

    unsafe fn dealloc(&self, pointer: *mut u8, layout: std::alloc::Layout) {
        assert!(
            !context::IN_NMI
                .try_with(|state| state.get())
                .unwrap_or(false)
        );
        // SAFETY: forwards the matching allocation and layout unchanged.
        unsafe { std::alloc::System.dealloc(pointer, layout) };
    }
}
extern crate self as kspin;
extern crate self as ktask;

pub mod context {
    std::thread_local! {
        pub static IN_NMI: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    pub fn is_in_nmi() -> bool {
        IN_NMI.get()
    }
}

pub mod irq_work {
    use std::{
        cell::RefCell,
        ptr::NonNull,
        sync::atomic::{AtomicBool, Ordering},
    };

    pub struct IrqWork {
        callback: unsafe fn(NonNull<Self>),
        claimed: AtomicBool,
    }

    impl IrqWork {
        pub const fn new(callback: unsafe fn(NonNull<Self>)) -> Self {
            Self {
                callback,
                claimed: AtomicBool::new(false),
            }
        }
    }

    std::thread_local! {
        static PENDING: RefCell<Vec<NonNull<IrqWork>>> = const { RefCell::new(Vec::new()) };
    }

    /// # Safety
    /// Matches the one-shot stable-storage contract of the real irq_work queue.
    pub unsafe fn queue(work: NonNull<IrqWork>) -> Result<(), ()> {
        // SAFETY: caller retains the published node through callback transfer.
        let node = unsafe { work.as_ref() };
        assert!(!node.claimed.swap(true, Ordering::AcqRel));
        PENDING.with_borrow_mut(|pending| {
            assert!(pending.len() < pending.capacity());
            pending.push(work);
        });
        Ok(())
    }

    pub fn prepare() {
        PENDING.with_borrow_mut(|pending| pending.reserve(16));
    }

    pub fn drain() -> usize {
        assert!(!super::context::is_in_nmi());
        let mut count = 0;
        while let Some(work) = PENDING.with_borrow_mut(Vec::pop) {
            // SAFETY: the one-shot queue transfers a live node and saves its
            // callback before it can free the allocation containing the node.
            let callback = unsafe { work.as_ref().callback };
            // SAFETY: this mock ordinary IRQ owns the transferred responsibility.
            unsafe { callback(work) };
            count += 1;
        }
        count
    }
}

use std::{
    panic::{AssertUnwindSafe, catch_unwind},
    pin::Pin,
    sync::{
        Arc, Barrier, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
};

#[path = "../../../src/finalize.rs"]
mod finalize;

pub enum DefaultQueue {
    Normal,
}

pub fn is_sleepable_context() -> bool {
    IS_SLEEPABLE.load(Ordering::Acquire)
}

static IS_SLEEPABLE: AtomicBool = AtomicBool::new(false);
static IS_READY: AtomicBool = AtomicBool::new(false);
static PAUSE_BIND: AtomicBool = AtomicBool::new(false);
static BIND_GATE: OnceLock<Barrier> = OnceLock::new();

pub struct SpinNoIrq<T>(Mutex<T>);

impl<T> SpinNoIrq<T> {
    pub fn new(value: T) -> Self {
        Self(Mutex::new(value))
    }

    pub fn lock(&self) -> MutexGuard<'_, T> {
        assert!(!context::is_in_nmi());
        self.0.lock().unwrap()
    }
}

pub struct Work<H = fn()> {
    callback: H,
}

impl<H> Work<H> {
    pub fn with_callback(callback: H) -> Self {
        Self { callback }
    }
}

pub struct WorkQueue;

#[derive(Debug)]
pub enum SubmitError {
    AlreadyQueued,
    Rejected,
}

static MODE: AtomicUsize = AtomicUsize::new(0);
static SUBMISSIONS: AtomicUsize = AtomicUsize::new(0);
static SLOT: Mutex<Option<fn()>> = Mutex::new(None);
static READY_PROBE: Mutex<Option<kfinalize::DeferredArc<usize>>> = Mutex::new(None);

impl WorkQueue {
    /// # Safety
    /// The borrowed work remains live until all accepted callbacks finish.
    pub unsafe fn queue_work(
        &self,
        work: Pin<&Work>,
        _target: Option<()>,
    ) -> Result<(), SubmitError> {
        SUBMISSIONS.fetch_add(1, Ordering::Relaxed);
        if MODE.load(Ordering::Relaxed) == 1 {
            return Err(SubmitError::Rejected);
        }
        {
            let mut slot = SLOT.lock().unwrap();
            if slot.is_some() {
                return Err(SubmitError::AlreadyQueued);
            }
            *slot = Some(work.callback);
        }
        if MODE.load(Ordering::Relaxed) == 2 {
            panic!("injected notification panic after acceptance");
        }
        if MODE.load(Ordering::Relaxed) == 3 {
            IS_SLEEPABLE.store(true, Ordering::Release);
            let owner = std::thread::spawn(|| kfinalize::DeferredArc::new(42usize).unwrap())
                .join()
                .unwrap();
            assert!(READY_PROBE.lock().unwrap().replace(owner).is_none());
        }
        Ok(())
    }
}

pub mod services {
    use super::{
        Arc, BIND_GATE, DefaultQueue, IS_READY, OnceLock, Ordering, PAUSE_BIND, WorkQueue,
    };

    pub fn with_default_queue<Output>(
        _kind: DefaultQueue,
        read: impl FnOnce(Option<&Arc<WorkQueue>>) -> Output,
    ) -> Output {
        static QUEUE: OnceLock<Arc<WorkQueue>> = OnceLock::new();
        if !IS_READY.load(Ordering::Acquire) {
            return read(None);
        }
        let result = read(Some(QUEUE.get_or_init(|| Arc::new(WorkQueue))));
        if PAUSE_BIND.swap(false, Ordering::AcqRel) {
            BIND_GATE.get().unwrap().wait();
            BIND_GATE.get().unwrap().wait();
        }
        result
    }
}

#[derive(Debug)]
struct Payload {
    drops: Arc<AtomicUsize>,
    should_panic: bool,
    publisher: std::thread::ThreadId,
}

impl Drop for Payload {
    fn drop(&mut self) {
        assert_ne!(std::thread::current().id(), self.publisher);
        assert_eq!(self.drops.fetch_add(1, Ordering::SeqCst), 0);
        assert!(!self.should_panic, "injected payload destructor panic");
    }
}

fn prepared(should_panic: bool) -> (kfinalize::DeferredArc<Payload>, Arc<AtomicUsize>) {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = kfinalize::DeferredArc::new(Payload {
        drops: drops.clone(),
        should_panic,
        publisher: std::thread::current().id(),
    })
    .unwrap();
    (owner, drops)
}

fn retained(drops: &Arc<AtomicUsize>) {
    assert_eq!(drops.load(Ordering::SeqCst), 0);
    assert_eq!(Arc::strong_count(drops), 2);
}

fn finished(drops: &Arc<AtomicUsize>) {
    assert_eq!(drops.load(Ordering::SeqCst), 1);
    assert_eq!(Arc::strong_count(drops), 1);
}

fn run_one(should_panic: bool) {
    let callback = SLOT.lock().unwrap().take().unwrap();
    assert_eq!(std::thread::spawn(callback).join().is_err(), should_panic);
}

fn notification_fault(mode: usize) {
    MODE.store(mode, Ordering::Relaxed);
    let (owner, drops) = prepared(false);
    let weak = kfinalize::DeferredArc::downgrade(&owner);
    assert_eq!(
        catch_unwind(AssertUnwindSafe(|| drop(owner))).is_err(),
        mode != 0
    );
    assert!(weak.upgrade().is_none());
    retained(&drops);
    assert_eq!(SLOT.lock().unwrap().is_some(), mode != 1);
    MODE.store(0, Ordering::Relaxed);
    let recovery = if mode == 1 {
        let (owner, counter) = prepared(false);
        drop(owner);
        Some(counter)
    } else {
        None
    };
    run_one(false);
    finished(&drops);
    if let Some(counter) = recovery {
        finished(&counter);
    }
    assert!(SLOT.lock().unwrap().is_none());
    println!("PASS notification mode={mode}: retained, weak invalid, dropped once");
}

fn coalescing_and_budget() {
    let start = SUBMISSIONS.load(Ordering::Relaxed);
    let counters: Vec<_> = (0..193)
        .map(|_| {
            let (owner, counter) = prepared(false);
            drop(owner);
            retained(&counter);
            counter
        })
        .collect();
    for batch in 0..4 {
        run_one(false);
        for (index, counter) in counters.iter().enumerate() {
            if index < ((batch + 1) * 64).min(193) {
                finished(counter);
            } else {
                retained(counter);
            }
        }
    }
    assert_eq!(SUBMISSIONS.load(Ordering::Relaxed) - start, 193 + 3);
    assert!(SLOT.lock().unwrap().is_none());
    println!("PASS coalescing and 64-item budget: 193 objects, four callbacks");
}

fn destructor_fault() {
    let (owner, failed) = prepared(true);
    let (tail, pending) = prepared(false);
    drop(owner);
    drop(tail);
    run_one(true);
    finished(&failed);
    retained(&pending);
    assert!(SLOT.lock().unwrap().is_none());
    let (trigger, trigger_drops) = prepared(false);
    drop(trigger);
    run_one(false);
    finished(&failed);
    finished(&pending);
    finished(&trigger_drops);
    assert!(SLOT.lock().unwrap().is_none());
    println!("PASS destructor panic: no retry, pending tail retained");
}

fn main() {
    boot_handoff();
    identity_layouts();
    raw_owners();
    #[cfg(feature = "scheduler-probe")]
    scheduler_owners();
    nmi_references();
    notification_fault(0);
    notification_fault(1);
    notification_fault(2);
    coalescing_and_budget();
    destructor_fault();
}

fn raw_owners() {
    use kfinalize::DeferredArc;

    #[repr(C, align(4096))]
    struct Linked {
        prefix: usize,
        link: AtomicUsize,
    }

    let owner = DeferredArc::new(Linked {
        prefix: 7,
        link: AtomicUsize::new(0),
    })
    .ok()
    .unwrap();
    let weak = DeferredArc::downgrade(&owner);
    let first = DeferredArc::into_raw(owner.clone());
    let second = DeferredArc::into_raw(owner.clone());
    assert_eq!(first.addr() % 4096, 0);
    assert_eq!(DeferredArc::strong_count(&owner), 3);
    // SAFETY: first remains an outstanding raw owner throughout both views;
    // neither view is consumed or explicitly dropped as an owning handle.
    let view = unsafe { DeferredArc::borrow_raw(first) };
    // SAFETY: the same independently retained raw owner also covers this view.
    let second_view = unsafe { DeferredArc::borrow_raw(first) };
    assert_eq!(DeferredArc::strong_count(&view), 3);
    let view_clone = DeferredArc::clone(&view);
    assert_eq!(DeferredArc::strong_count(&second_view), 4);
    drop(view_clone);
    assert_eq!(DeferredArc::strong_count(&owner), 3);
    let borrowed = &owner.link;
    borrowed.store(11, Ordering::Release);
    // SAFETY: first is a live raw owner; projecting its real link field and
    // reversing that offset preserves the original payload provenance.
    let link = unsafe { core::ptr::addr_of!((*first).link) };
    // SAFETY: reversing the real field offset stays within the same allocation.
    let base = unsafe { link.cast::<u8>().sub(core::mem::offset_of!(Linked, link)) };
    // SAFETY: base preserves first's original provenance and unconsumed count.
    let recovered = unsafe { DeferredArc::from_raw(base.cast::<Linked>()) };
    assert!(DeferredArc::ptr_eq(&owner, &recovered));
    assert_eq!(DeferredArc::strong_count(&owner), 3);
    assert_eq!(borrowed.load(Ordering::Acquire), 11);
    drop(recovered);
    drop(owner);
    assert!(weak.upgrade().is_some());
    // SAFETY: second retains its original, independently transferred count.
    drop(unsafe { DeferredArc::from_raw(second) });
    assert!(weak.upgrade().is_none());
    drop(weak);
    run_one(false);

    let zero = DeferredArc::new(()).unwrap();
    let raw = DeferredArc::into_raw(zero);
    // SAFETY: raw transfers the sole live owner, including for a zero-sized T.
    drop(unsafe { DeferredArc::from_raw(raw) });
    run_one(false);

    let (owner, drops) = prepared(false);
    let raw = DeferredArc::into_raw(owner);
    irq_work::prepare();
    context::IN_NMI.set(true);
    // SAFETY: raw still owns the original count; local mock irq_work is ready.
    let recovered = unsafe { DeferredArc::from_raw(raw) };
    let clone = recovered.clone();
    assert!(DeferredArc::ptr_eq(&clone, &recovered));
    drop(clone);
    drop(recovered);
    context::IN_NMI.set(false);
    retained(&drops);
    assert_eq!(irq_work::drain(), 2);
    run_one(false);
    finished(&drops);
    println!(
        "PASS raw owners: counts, intrusive offset roundtrip, alignment, ZST, NMI final owner"
    );
}

#[cfg(feature = "scheduler-probe")]
fn scheduler_owners() {
    use kfinalize::DeferredArc;
    use ksched::{
        DlParams, EnqueueKind, KernelClass, RtPrio, SchedAttr, SchedPolicy, SchedTask, Scheduler,
    };

    type Owner = DeferredArc<SchedTask<usize>>;
    println!(
        "scheduler layout: arc_scheduler={} deferred_scheduler={} entity={} arc_owner={} \
         deferred_owner={}",
        core::mem::size_of::<Scheduler<usize>>(),
        core::mem::size_of::<Scheduler<usize, Owner>>(),
        core::mem::size_of::<SchedTask<usize>>(),
        core::mem::size_of::<Arc<SchedTask<usize>>>(),
        core::mem::size_of::<Owner>()
    );
    let rt_only = std::env::args().any(|argument| argument == "--rt-only");
    for attr in [
        SchedAttr::DEFAULT,
        SchedAttr {
            policy: SchedPolicy::Fifo,
            rt_priority: RtPrio::new(1).unwrap(),
            ..SchedAttr::DEFAULT
        },
        SchedAttr {
            policy: SchedPolicy::Deadline,
            dl_params: Some(DlParams::new(1_000_000, 10_000_000, 10_000_000).unwrap()),
            ..SchedAttr::DEFAULT
        },
    ] {
        if rt_only && attr.policy != SchedPolicy::Fifo {
            continue;
        }
        let idle = Owner::new(SchedTask::new_kernel(0, KernelClass::Idle))
            .ok()
            .unwrap();
        let mut source = Scheduler::<usize, Owner>::new(idle.clone(), 0);
        let mut destination = Scheduler::<usize, Owner>::new(idle.clone(), 0);
        let task = Owner::new(SchedTask::new(13)).ok().unwrap();
        let weak = Owner::downgrade(&task);
        source.set_attr(&task, attr, 0).unwrap();
        let _ = source.enqueue(task.clone(), EnqueueKind::New, 0);
        let count = Owner::strong_count(&task);
        assert!(source.steal(&mut |_| false, 0).is_none());
        assert_eq!(Owner::strong_count(&task), count);
        if attr.policy == SchedPolicy::Deadline {
            assert!(source.steal(&mut |_| true, 0).is_none());
            drop(source.pick_next(0));
            let _ = source.update(1_000_000, ksched::UpdateKind::Account);
            source.preempt_current(1_000_000);
            assert_eq!(source.nr_queued(), 0);
            let _ = source.update(10_000_000, ksched::UpdateKind::Account);
            assert_eq!(source.nr_queued(), 1);
            drop(source);
            assert_eq!(Owner::strong_count(&task), 1);
            drop(task);
            assert!(weak.upgrade().is_none());
            drop(weak);
            drop(destination);
            drop(idle);
            run_one(false);
            continue;
        }
        let stolen = source.steal(&mut |_| true, 0).unwrap();
        assert!(Owner::ptr_eq(&task, &stolen));
        assert_eq!(Owner::strong_count(&task), 2);
        let _ = destination.enqueue(stolen, EnqueueKind::MigrateIn, 0);
        drop(destination.dequeue(&task, 0).unwrap());
        assert_eq!(Owner::strong_count(&task), 1);
        let _ = source.enqueue(task.clone(), EnqueueKind::MigrateIn, 0);
        drop(source);
        assert_eq!(Owner::strong_count(&task), 1);
        destination.install_current(task.clone(), 0);
        drop(task);
        let exited = destination.exit_current(1);
        assert_eq!(Owner::strong_count(&exited), 1);
        drop(exited);
        assert!(weak.upgrade().is_none());
        drop(weak);
        drop(destination);
        drop(idle);
        run_one(false);
    }
    println!(
        "PASS scheduler owners: rt_only={rt_only}, real adapters, ownership transfer and teardown"
    );
}

fn identity_layouts() {
    use std::alloc::Layout;
    irq_work::prepare();
    for alignment in [1, 8, 64, 4096] {
        for size in [0, 1, 17, 255] {
            let layout = Layout::from_size_align(size, alignment).unwrap();
            let allocation = (finalize::IDENTITY_MEMORY.allocate)(layout).unwrap();
            let pointer = allocation.region.cast::<u8>();
            assert_eq!(pointer.as_ptr().addr() % alignment, 0);
            // SAFETY: the returned region contains exactly size writable bytes.
            unsafe { pointer.as_ptr().write_bytes(0x5a, size) };
            context::IN_NMI.set(true);
            // SAFETY: transfers the provider's original token and matching layout.
            unsafe { (finalize::IDENTITY_MEMORY.deallocate)(allocation.token, layout) };
            context::IN_NMI.set(false);
            assert_eq!(irq_work::drain(), 1);
        }
    }
    let overflowing = Layout::from_size_align(isize::MAX as usize, 1).unwrap();
    assert!((finalize::IDENTITY_MEMORY.allocate)(overflowing).is_err());
    println!(
        "PASS identity layout: zero/small sizes, alignment through 4096, overflow rejected, NMI \
         free deferred"
    );
}

fn nmi_references() {
    irq_work::prepare();
    for retain_weak in [false, true] {
        let (owner, drops) = prepared(false);
        let weak = retain_weak.then(|| kfinalize::DeferredArc::downgrade(&owner));
        context::IN_NMI.set(true);
        drop(owner);
        if let Some(weak) = &weak {
            assert!(weak.upgrade().is_none());
        }
        context::IN_NMI.set(false);
        assert_eq!(drops.load(Ordering::Acquire), 0);
        assert!(SLOT.lock().unwrap().is_none());
        assert_eq!(irq_work::drain(), if retain_weak { 1 } else { 2 });
        run_one(false);
        finished(&drops);
        if let Some(weak) = weak {
            context::IN_NMI.set(true);
            drop(weak);
            context::IN_NMI.set(false);
            assert_eq!(irq_work::drain(), 1);
            assert!(SLOT.lock().unwrap().is_none());
        }
    }
    println!(
        "PASS NMI: last strong without Weak, expired Weak before payload completion, late last \
         Weak; no allocation/free in NMI"
    );
}

fn boot_handoff() {
    let input = Arc::new(42usize);
    // SAFETY: host allocation and local locking are initialized; no IRQ handler.
    let result = unsafe { kfinalize::service::new_integration(input.clone()) };
    let (error, returned) = result.err().unwrap();
    assert_eq!(error, kfinalize::CreateError::NotReady);
    assert!(Arc::ptr_eq(&input, &returned));
    drop(returned);
    assert_eq!(Arc::strong_count(&input), 1);
    // SAFETY: serialized provider installation precedes all producer threads.
    unsafe { finalize::init_preparation() };
    assert_eq!(SUBMISSIONS.load(Ordering::Relaxed), 0);
    let mut owners = Vec::new();
    let mut counters = Vec::new();
    let mut weaks = Vec::new();
    for _ in 0..3 {
        let drops = Arc::new(AtomicUsize::new(0));
        // SAFETY: the host fixture provides an initialized allocator and
        // mutex-backed IRQ-lock substitute, with serialized startup here.
        let owner = unsafe {
            kfinalize::service::new_integration(Payload {
                drops: drops.clone(),
                should_panic: false,
                publisher: std::thread::current().id(),
            })
        }
        .unwrap();
        weaks.push(kfinalize::DeferredArc::downgrade(&owner));
        owners.push(owner);
        counters.push(drops);
    }
    drop(owners.pop());
    finalize::install();
    assert!(SLOT.lock().unwrap().is_none());
    assert!(matches!(
        kfinalize::DeferredArc::new(1usize),
        Err((kfinalize::CreateError::NotReady, 1))
    ));
    for drops in &counters {
        retained(drops);
    }
    BIND_GATE.set(Barrier::new(2)).unwrap();
    IS_READY.store(true, Ordering::Release);
    MODE.store(3, Ordering::Relaxed);
    PAUSE_BIND.store(true, Ordering::Release);
    let binder = std::thread::spawn(finalize::install);
    BIND_GATE.get().unwrap().wait();
    drop(owners.pop());
    let producers: Vec<_> = (0..4)
        .map(|_| {
            std::thread::spawn(|| {
                (0..8)
                    .map(|_| {
                        let drops = Arc::new(AtomicUsize::new(0));
                        // SAFETY: host threads have allocation and locking support;
                        // the permanent provider was published before spawning.
                        let owner = unsafe {
                            kfinalize::service::new_integration(Payload {
                                drops: drops.clone(),
                                should_panic: false,
                                publisher: std::thread::current().id(),
                            })
                        }
                        .unwrap();
                        let weak = kfinalize::DeferredArc::downgrade(&owner);
                        drop(owner);
                        assert!(weak.upgrade().is_none());
                        retained(&drops);
                        drops
                    })
                    .collect::<Vec<_>>()
            })
        })
        .collect();
    for producer in producers {
        counters.extend(producer.join().unwrap());
    }
    assert!(SLOT.lock().unwrap().is_none());
    BIND_GATE.get().unwrap().wait();
    binder.join().unwrap();
    MODE.store(0, Ordering::Relaxed);
    drop(READY_PROBE.lock().unwrap().take());
    drop(owners.pop());
    assert!(weaks.iter().all(|weak| weak.upgrade().is_none()));
    for drops in &counters {
        retained(drops);
    }
    run_one(false);
    for drops in &counters {
        finished(drops);
    }
    IS_SLEEPABLE.store(true, Ordering::Release);
    assert!(SLOT.lock().unwrap().is_none());
    println!(
        "PASS boot: input retained, concurrent preparation, before/during/after bind retire once"
    );
}
