// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Prepared intrusive nodes drained by a permanent normal work item.

use alloc::{alloc::Global, sync::Arc};
use core::{
    alloc::{AllocError, Allocator, Layout},
    cell::UnsafeCell,
    pin::Pin,
    ptr::NonNull,
};

use intrusive_collections::{LinkedList, LinkedListAtomicLink, intrusive_adapter};
use kfinalize::{
    CreateError,
    service::{Finalization, Prepared},
};
use kirq::irq_work::{self, IrqWork};
use klazy::Once;
use kspin::SpinNoIrq;

use crate::{DefaultQueue, SubmitError, Work, WorkQueue, services};

const BATCH_LIMIT: usize = 64;

static NORMAL: Once<Executor> = Once::new();

struct Executor {
    work: Work<fn()>,
    queue: Once<Arc<WorkQueue>>,
    pending: SpinNoIrq<LinkedList<CarrierAdapter>>,
}

#[repr(C)]
struct Carrier {
    irq: IrqWork,
    links: LinkedListAtomicLink,
    executor: &'static Executor,
    finalization: UnsafeCell<Option<Finalization>>,
}

#[repr(C)]
struct IdentityHeader {
    irq: IrqWork,
    allocation_layout: Layout,
}

pub(super) static IDENTITY_MEMORY: kfinalize::service::IdentityMemory =
    kfinalize::service::IdentityMemory {
        allocate: allocate_identity,
        deallocate: release_identity,
    };

fn identity_layout(layout: Layout) -> Result<(Layout, usize), AllocError> {
    Layout::new::<IdentityHeader>()
        .extend(layout)
        .map_err(|_| AllocError)
}

fn allocate_identity(layout: Layout) -> Result<kfinalize::service::IdentityAllocation, AllocError> {
    let (allocation_layout, offset) = identity_layout(layout)?;
    let base = Global.allocate(allocation_layout)?.cast::<u8>();
    let header = IdentityHeader {
        irq: IrqWork::new(release_identity_irq),
        allocation_layout,
    };
    // SAFETY: the extended allocation includes an aligned, disjoint header.
    unsafe { base.cast::<IdentityHeader>().as_ptr().write(header) };
    // SAFETY: Layout::extend places the requested region within the allocation.
    let pointer = unsafe { base.add(offset) };
    Ok(kfinalize::service::IdentityAllocation {
        region: NonNull::slice_from_raw_parts(pointer, layout.size()),
        token: base.cast(),
    })
}

/// # Safety
/// token is the original release token from allocate_identity, transferred once.
unsafe fn release_identity(token: NonNull<()>, _layout: Layout) {
    let base = token.cast::<u8>();
    if kirq::context::is_in_nmi() {
        // SAFETY: Arc has ended all accesses, including the implicit Weak.
        // The independent prefix owns this allocation through IRQ consumption.
        let result = unsafe { irq_work::queue(base.cast()) };
        assert!(
            result.is_ok(),
            "NMI identity retirement requires local irq_work readiness"
        );
    } else {
        // SAFETY: the separately retained token covers the independent prefix.
        let allocation_layout = unsafe { base.cast::<IdentityHeader>().as_ref().allocation_layout };
        // SAFETY: Arc transferred the complete allocation, no references remain.
        unsafe { Global.deallocate(base, allocation_layout) };
    }
}

/// # Safety
/// work is the first field of the uniquely retired IdentityHeader allocation.
unsafe fn release_identity_irq(work: NonNull<IrqWork>) {
    // SAFETY: the prefix is outside Arc's destroyed region and remains live.
    let layout = unsafe { work.cast::<IdentityHeader>().as_ref().allocation_layout };
    // SAFETY: the dispatcher has unlinked this one-shot node and will not touch it.
    unsafe { Global.deallocate(work.cast(), layout) };
}

/// # Safety
/// work is Carrier's first field, published with one Arc::into_raw owner.
unsafe fn submit_carrier_irq(work: NonNull<IrqWork>) {
    // SAFETY: the raw strong owner was transferred once, without a decrement.
    let carrier = unsafe { Arc::from_raw(work.cast::<Carrier>().as_ptr()) };
    carrier.publish();
}

impl Carrier {
    fn publish(self: Arc<Self>) {
        #[cfg(all(unittest, feature = "system"))]
        let publication_guard = crate::finalize_unittest::publication_guard();
        self.executor.pending.lock().push_back(self.clone());
        self.executor.notify();
        #[cfg(all(unittest, feature = "system"))]
        drop(publication_guard);
        #[cfg(all(unittest, feature = "system"))]
        crate::finalize_unittest::after_publish(|| Arc::strong_count(&self));
    }
}

intrusive_adapter!(CarrierAdapter = Arc<Carrier>: Carrier { links => LinkedListAtomicLink });

// SAFETY: the sole submit initializes the token before publishing under pending's
// IRQ-safe lock. List ownership and that lock serialize links. Only the consumer
// removes the node and takes its token; the publisher never accesses it again.
unsafe impl Sync for Carrier {}

#[cfg(all(unittest, feature = "system"))]
pub(super) fn carrier_layout_for_test() -> (usize, usize) {
    (
        core::mem::size_of::<Carrier>(),
        core::mem::align_of::<Carrier>(),
    )
}

impl Executor {
    fn new(callback: fn()) -> Self {
        Self {
            work: Work::with_callback(callback),
            queue: Once::new(),
            pending: SpinNoIrq::new(LinkedList::new(CarrierAdapter::new())),
        }
    }

    fn notify(&'static self) {
        let Some(queue) = self.queue.get() else {
            return;
        };
        // SAFETY: the Once retains this private Work and captures for kernel life.
        // It cannot be canceled, disabled or moved; its fixed queue selects the
        // audited callback context. Notifications and callbacks hold no list lock.
        let work = unsafe { Pin::new_unchecked(&self.work) };
        // SAFETY: permanent storage covers concurrent submit and all successors.
        // None retains unfinished CPU ownership when the shared Work is running.
        let result = unsafe { queue.queue_work(work, None) };
        assert!(
            result.is_ok() || matches!(result, Err(SubmitError::AlreadyQueued)),
            "finalization notification failed: {result:?}"
        );
    }

    fn run(&'static self) {
        for _ in 0..BATCH_LIMIT {
            let carrier = self.pending.lock().pop_front();
            let Some(carrier) = carrier else {
                #[cfg(all(unittest, feature = "system"))]
                crate::finalize_unittest::before_empty_return();
                return;
            };
            // SAFETY: removal transfers the unique list owner after publication
            // synchronized through pending. No producer reads or writes this cell
            // after enqueue, and no other callback can obtain this one-shot node.
            let finalization = unsafe { (*carrier.finalization.get()).take() };
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: one-shot submit initializes the \
                          token before list publication; only the removing consumer takes it"
            )]
            let finalization = finalization.expect("queued finalization owns its token");
            // SAFETY: this callback runs on the permanent Normal queue, outside
            // the list lock. Public owners ended; this sleepable worker owns
            // the sole token and may destroy the payload and its fields.
            unsafe { finalization.finish() };
        }
        let has_pending = !self.pending.lock().is_empty();
        if has_pending {
            self.notify();
        }
    }
}

pub(crate) fn install() {
    let normal = services::with_default_queue(DefaultQueue::Normal, |queue| queue.cloned());
    let Some(normal) = normal else {
        return;
    };
    let executor = NORMAL.call_once(|| Executor::new(run_normal));
    // SAFETY: permanent allocator pair is installed before exposing preparation.
    unsafe { kfinalize::service::install_identity_memory(&IDENTITY_MEMORY) };
    // SAFETY: default services are permanent and ready. Publish preparation
    // before enabling callbacks so an early destructor can prepare new owners.
    // Until queue binding below, the permanent pending list retains submissions.
    unsafe { kfinalize::service::install_normal(prepare) };
    executor.queue.call_once(|| normal);
    let has_pending = !executor.pending.lock().is_empty();
    if has_pending {
        executor.notify();
    }
}

fn run_normal() {
    if let Some(executor) = NORMAL.get() {
        executor.run();
    }
}

fn prepare() -> Result<Arc<dyn Prepared>, CreateError> {
    if !ktask::is_sleepable_context() {
        return Err(CreateError::InvalidContext);
    }
    let executor = NORMAL.get().ok_or(CreateError::NotReady)?;
    Ok(prepare_on(executor))
}

fn prepare_on(executor: &'static Executor) -> Arc<dyn Prepared> {
    Arc::new(Carrier {
        irq: IrqWork::new(submit_carrier_irq),
        links: LinkedListAtomicLink::new(),
        executor,
        finalization: UnsafeCell::new(None),
    })
}

/// Installs pure preparation before scheduler and worker startup.
///
/// This creates no tasks and does not bind execution. Integration users call
/// kfinalize::service::new_integration after publication. Retired payloads stay
/// on the permanent list until successful normal-service installation.
///
/// # Safety
/// Call during serialized primary startup after allocator/per-CPU/IRQ-lock
/// support is ready and before exposing integration users on any CPU. Consumers
/// must establish their own local prerequisites before allocating. No teardown
/// is supported and startup must not wait for physical reclamation.
///
/// # Panics
/// Poisoned startup initialization follows kernel policy.
pub unsafe fn init_preparation() {
    NORMAL.call_once(|| Executor::new(run_normal));
    // SAFETY: startup is serialized and precedes all production preparations.
    unsafe { kfinalize::service::install_identity_memory(&IDENTITY_MEMORY) };
    // SAFETY: the permanent executor is published first. Each invocation
    // allocates a fresh node and the synchronized list retains unbound tokens.
    // This provider neither queries current Task nor waits for execution.
    unsafe { kfinalize::service::install_integration(prepare_integration) };
}

/// Prepares a carrier without consulting task-context state.
///
/// # Safety
/// The caller must satisfy kfinalize::service::new_integration's local
/// allocation and locking prerequisites, outside hardirq, softirq and NMI.
unsafe fn prepare_integration() -> Result<Arc<dyn Prepared>, CreateError> {
    let executor = NORMAL.get().ok_or(CreateError::NotReady)?;
    Ok(prepare_on(executor))
}

// SAFETY: each private node is submitted once to its fixed executor. The list's
// Arc retains it independently of the public identity and publishing Arc. Payload
// destruction occurs only after removal, outside locks; panic never inlines Drop.
unsafe impl Prepared for Carrier {
    unsafe fn submit(self: Arc<Self>, finalization: Finalization) {
        // SAFETY: Prepared requires one submission; no reader can reach the cell
        // before the following list publication, after which it is consumer-only.
        unsafe { *self.finalization.get() = Some(finalization) };
        if kirq::context::is_in_nmi() {
            let pointer = Arc::into_raw(self).cast_mut();
            // SAFETY: Arc::into_raw preserves the sole transferred strong owner.
            let work = unsafe { NonNull::new_unchecked(pointer).cast() };
            // SAFETY: the first-field one-shot node owns that raw Arc until IRQ.
            let result = unsafe { irq_work::queue(work) };
            assert!(
                result.is_ok(),
                "NMI payload retirement requires local irq_work readiness"
            );
        } else {
            self.publish();
        }
    }
}
