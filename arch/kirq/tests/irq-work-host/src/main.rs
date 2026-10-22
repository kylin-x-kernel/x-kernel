// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host-only execution model for the real local IRQ-work queue.

extern crate self as kcpu_slot;
extern crate self as kspin;
extern crate self as unittest;

use std::{cell::Cell, ptr::NonNull, sync::Barrier, thread};

const CPU_COUNT: usize = 4;

thread_local! {
    static CPU_INDEX: Cell<Option<usize>> = const { Cell::new(None) };
    static NOTIFICATIONS: Cell<usize> = const { Cell::new(0) };
    static COMPLETED: Cell<usize> = const { Cell::new(0) };
    static FREED: Cell<usize> = const { Cell::new(0) };
    static CALLBACK_ACTIVE: Cell<bool> = const { Cell::new(false) };
}

fn current_cpu() -> usize {
    CPU_INDEX.get().expect("worker must own a CPU")
}

/// Marker matching the production CPU-slot template contract.
///
/// # Safety
/// Implementors must support independent static initialization without owning
/// external resources or containing references into a different slot.
pub unsafe trait StaticSlotValue {}

mod macros {
    /// Generates four permanent, independently initialized slots without heap leaks.
    #[macro_export]
    macro_rules! cpu_slot {
        (static $name:ident : $value:ty = $init:expr;) => {
            struct HostSlot;
            static $name: HostSlot = HostSlot;
            pub(crate) fn host_local_layout() -> (usize, usize) {
                (
                    std::mem::size_of::<$value>(),
                    std::mem::align_of::<$value>(),
                )
            }
            impl HostSlot {
                fn current_shared(&self) -> &'static $value {
                    fn require_slot_value<Value: $crate::StaticSlotValue>() {}
                    require_slot_value::<$value>();
                    static VALUES: [$value; $crate::CPU_COUNT] =
                        [const { $init }; $crate::CPU_COUNT];
                    &VALUES[$crate::current_cpu()]
                }
            }
        };
    }

    /// Ignores kernel UT modules; this binary supplies its own scenarios.
    #[macro_export]
    macro_rules! path_mod_test {
        ($($tokens:tt)*) => {};
    }
}

/// No-op: each CPU is exclusively owned by one host thread.
pub struct NoPreemptIrqSave;

impl NoPreemptIrqSave {
    /// Host threads never migrate between modeled CPUs.
    pub fn new() -> Self {
        Self
    }
}

mod context {
    use std::cell::Cell;

    thread_local! {
        static IN_NMI: Cell<bool> = const { Cell::new(false) };
        static IN_HARDIRQ: Cell<bool> = const { Cell::new(false) };
    }

    pub fn is_in_nmi() -> bool {
        IN_NMI.get()
    }

    pub fn is_in_hardirq() -> bool {
        IN_HARDIRQ.get()
    }

    pub fn is_in_interrupt_context() -> bool {
        is_in_nmi() || is_in_hardirq()
    }

    struct Restore<'a>(&'a Cell<bool>, bool);

    impl Drop for Restore<'_> {
        fn drop(&mut self) {
            self.0.set(self.1);
        }
    }

    pub fn nmi(action: impl FnOnce()) {
        IN_NMI.with(|state| {
            let _restore = Restore(state, state.replace(true));
            action();
        });
    }

    pub fn hardirq(action: impl FnOnce()) {
        IN_HARDIRQ.with(|state| {
            let _restore = Restore(state, state.replace(true));
            action();
        });
    }
}

mod platform {
    pub struct IntrManagerIf;

    impl IntrManagerIf {
        pub fn prepare_local_irq_work() -> Option<fn()> {
            Some(|| super::NOTIFICATIONS.set(super::NOTIFICATIONS.get() + 1))
        }
    }
}

#[path = "../../../src/irq_work.rs"]
mod irq_work;

use irq_work::{IrqWork, QueueError};

fn record_callback() {
    assert!(context::is_in_hardirq());
    assert!(!context::is_in_nmi());
    COMPLETED.set(COMPLETED.get() + 1);
}

/// Records dispatch without taking ownership of the retained test allocation.
///
/// # Safety
/// The caller must retain the live node until dispatch returns and invoke this
/// callback only in ordinary hardirq context on its owning thread.
unsafe fn retained_callback(_work: NonNull<IrqWork>) {
    record_callback();
}

#[repr(C)]
struct OwnedNode {
    work: IrqWork,
    cpu: usize,
    child: Option<Box<OwnedNode>>,
}

impl OwnedNode {
    fn new(child: Option<Box<Self>>) -> Box<Self> {
        Box::new(Self {
            work: IrqWork::new(free_callback),
            cpu: current_cpu(),
            child,
        })
    }
}

impl Drop for OwnedNode {
    fn drop(&mut self) {
        assert_eq!(self.cpu, current_cpu());
        FREED.set(FREED.get() + 1);
    }
}

fn submit_owned(node: Box<OwnedNode>) {
    let work = NonNull::new(Box::into_raw(node)).unwrap().cast::<IrqWork>();
    // SAFETY: repr(C) puts work at offset zero in this stable Box allocation.
    // The owning CPU cannot consume inline; successful dispatch takes ownership.
    let result = unsafe { irq_work::queue(work) };
    if result.is_err() {
        // SAFETY: failed publication transfers no ownership; this fresh node has
        // no other submitters, so its original Box is still exclusively ours.
        drop(unsafe { Box::from_raw(work.cast::<OwnedNode>().as_ptr()) });
    }
    assert_eq!(result, Ok(()));
}

/// Frees the unlinked allocation and optionally publishes a prepared child.
///
/// # Safety
/// work must be the first field of a uniquely transferred Box<OwnedNode> that
/// has been unlinked on its owning CPU in ordinary hardirq context.
unsafe fn free_callback(work: NonNull<IrqWork>) {
    assert!(!CALLBACK_ACTIVE.replace(true));
    // SAFETY: submit_owned transferred this Box exactly once; repr(C) preserves
    // its base address and dispatch has stopped accessing the unlinked node.
    let mut node = unsafe { Box::from_raw(work.cast::<OwnedNode>().as_ptr()) };
    assert_eq!(node.cpu, current_cpu());
    record_callback();
    if let Some(child) = node.child.take() {
        let completed = COMPLETED.get();
        let notifications = NOTIFICATIONS.get();
        context::nmi(|| {
            submit_owned(child);
            assert_eq!(NOTIFICATIONS.get(), notifications + 1);
            irq_work::handle_irq();
            assert_eq!(COMPLETED.get(), completed);
        });
        irq_work::handle_irq();
        assert_eq!(COMPLETED.get(), completed);
    }
    drop(node);
    CALLBACK_ACTIVE.set(false);
}

fn exercise_cpu(barrier: &Barrier) {
    assert!(!irq_work::is_ready());
    let retained = NonNull::new(Box::into_raw(Box::new(IrqWork::new(retained_callback)))).unwrap();
    // SAFETY: this stable allocation remains retained until all explicit drains
    // and duplicate probes finish; no callback frees it or moves it.
    let result = unsafe { irq_work::queue(retained) };
    assert_eq!(result, Err(QueueError::NotReady));
    assert_eq!(NOTIFICATIONS.get(), 0);
    context::hardirq(|| assert!(!irq_work::init_current_cpu()));
    context::nmi(|| assert!(!irq_work::init_current_cpu()));
    assert!(irq_work::init_current_cpu());
    assert!(irq_work::init_current_cpu());
    assert!(irq_work::is_ready());
    barrier.wait();

    // SAFETY: the retained allocation is live and stable through dispatch.
    assert_eq!(unsafe { irq_work::queue(retained) }, Ok(()));
    // SAFETY: the same owner protects the allocation through this duplicate
    // probe; ordinary consumption only occurs explicitly on this thread.
    let result = unsafe { irq_work::queue(retained) };
    assert_eq!(result, Err(QueueError::AlreadyQueued));
    assert_eq!(NOTIFICATIONS.get(), 1);
    assert_eq!(COMPLETED.get(), 0);
    irq_work::handle_irq();
    assert_eq!(COMPLETED.get(), 0);
    context::hardirq(|| context::nmi(irq_work::handle_irq));
    assert_eq!(COMPLETED.get(), 0);
    context::hardirq(irq_work::handle_irq);
    assert_eq!(COMPLETED.get(), 1);
    // SAFETY: retained_callback leaves the allocation live; the one-shot node
    // must reject reuse even after its original callback has completed.
    let result = unsafe { irq_work::queue(retained) };
    assert_eq!(result, Err(QueueError::AlreadyQueued));
    // SAFETY: dispatch completed, no queue holds this node, and the callback
    // never took ownership; reconstruct the original Box exactly once.
    drop(unsafe { Box::from_raw(retained.as_ptr()) });

    COMPLETED.set(0);
    NOTIFICATIONS.set(0);
    assert_eq!(irq_work::CALLBACK_BUDGET, 64);
    for _ in 0..193 {
        submit_owned(OwnedNode::new(None));
    }
    assert_eq!(COMPLETED.get(), 0);
    assert_eq!(FREED.get(), 0);
    assert_eq!(NOTIFICATIONS.get(), 193);
    barrier.wait();
    for (completed, notifications) in [(64, 194), (128, 195), (192, 196), (193, 196)] {
        context::hardirq(irq_work::handle_irq);
        assert_eq!(COMPLETED.get(), completed);
        assert_eq!(FREED.get(), completed);
        assert_eq!(NOTIFICATIONS.get(), notifications);
    }

    let parent = OwnedNode::new(Some(OwnedNode::new(None)));
    context::nmi(|| {
        submit_owned(parent);
        irq_work::handle_irq();
        assert_eq!(COMPLETED.get(), 193);
    });
    context::hardirq(irq_work::handle_irq);
    assert_eq!(COMPLETED.get(), 195);
    assert_eq!(FREED.get(), 195);
    assert_eq!(NOTIFICATIONS.get(), 198);
    context::hardirq(irq_work::handle_irq);
    assert_eq!(COMPLETED.get(), 195);
    assert_eq!(NOTIFICATIONS.get(), 198);
    barrier.wait();
}

fn main() {
    let (local_bytes, local_align) = irq_work::host_local_layout();
    println!(
        "LAYOUT: IrqWork={} bytes align={}; LocalState={} bytes align={}; {} slots={} bytes",
        std::mem::size_of::<IrqWork>(),
        std::mem::align_of::<IrqWork>(),
        local_bytes,
        local_align,
        CPU_COUNT,
        CPU_COUNT * local_bytes,
    );
    let barrier = Barrier::new(CPU_COUNT);
    thread::scope(|scope| {
        for cpu in 0..CPU_COUNT {
            let barrier = &barrier;
            scope.spawn(move || {
                assert_eq!(CPU_INDEX.replace(Some(cpu)), None);
                exercise_cpu(barrier);
            });
        }
    });
    println!(
        "PASS: 4 CPUs; NotReady, one-shot duplicates, 193-node budget, NMI exclusion, nested NMI \
         publication, self-free and CPU isolation"
    );
}
