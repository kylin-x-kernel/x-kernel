// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Shared execution scheduling of queue CPU partitions, not individual Work.
//!
//! Each PartitionSource represents one association with this dispatch list. Its
//! payload supplies the management-side partition adapter. Publishers retain
//! the source; a queued turn owns another Arc, transferred to PartitionInspection
//! when selected. Idle sources are not retained by the dispatcher. The source's
//! Weak dispatcher identity prevents insertion into a different list without
//! creating a source -> dispatcher -> source strong-reference cycle.
//!
//! One IRQ-safe lock protects both list membership and every source's demand
//! state. Inspection accesses the payload after unlocking. Finishing returns a
//! still-demanded source to the tail before a later layer invokes acquired work.
//! Dropping an unfinished inspection conservatively requeues its demand: this
//! guard owns only a source turn, NEVER an already-claimed Work execution. Once
//! detached, unfinished turn Drop retires its reference without requeueing.
//!
//! Only a retained-demand hint is returned here. The runtime must reconcile demand
//! with parked workers/BH continuation under its scheduling protocol. This unit
//! does not implement that protocol or Queue shutdown wiring. The management layer
//! owns the concrete queue adapter. A live publisher must retain the service independently.
//! Dropping a dispatcher releases its list without a held spinlock; it is not Queue shutdown.
//! Only dispatcher/source types cross the crate boundary for management binding.
//!
//! Detach rejects later notices and removes queued demand under the service lock.
//! Its independent completion also waits for references being released outside
//! that lock. Only the first detach owns completion closure, so a repeated caller
//! cannot acknowledge ahead of its unlocked removal cleanup. Inspection retirement
//! and detachment synchronize through SourceDetachment; notification delivery occurs
//! after all service/control guards. Acquired callback completion is separate.

use alloc::sync::{Arc, Weak};
use core::cell::UnsafeCell;

use intrusive_collections::{LinkedList, intrusive_adapter, linked_list::AtomicLink};
use kpoll::Completion;
use kspin::SpinNoIrq;

unittest::path_mod_test!(dispatch_unittest, "../ut-tests/dispatch_unittest.rs");

use crate::{
    execution::{AcquiredExecution, PartitionWorkSource, SourceRemainder},
    source::PartitionDispatchState,
    source_detachment::SourceDetachment,
};

/// One shared service's FIFO of partition sources awaiting inspection.
pub struct PartitionDispatch<S> {
    sources: SpinNoIrq<LinkedList<SourceAdapter<S>>>,
}

/// Stable source identity and demand for one queue partition association.
pub struct PartitionSource<S> {
    owner: Weak<PartitionDispatch<S>>,
    links: AtomicLink,
    state: UnsafeCell<PartitionDispatchState>,
    detachment: Arc<SourceDetachment>,
    payload: S,
}

// SAFETY: The private state is accessed only while its owning dispatcher's
// sources lock is held, with ownership checked before notify. Other transitions
// use nodes taken from that same list. Payload access is shared and requires
// Sync; ownership may move between threads only when S is also Send. Links are
// managed by the intrusive list under that lock and never exposed separately.
unsafe impl<S: Send + Sync> Sync for PartitionSource<S> {}

intrusive_adapter!(SourceAdapter<S> = Arc<PartitionSource<S>>: PartitionSource<S> {
    links => AtomicLink
});

impl<S> PartitionSource<S> {
    /// Obtain the independent one-shot receipt before any detach effect, so a
    /// wait adapter can finish fallible registration first. Observe/register only:
    /// callers must never complete or reset this dispatcher-owned receipt.
    pub fn detachment_completion(&self) -> Arc<Completion> {
        self.detachment.completion()
    }
}

/// Sole inspection turn; the borrow retains the service, the Arc retains source.
#[must_use = "finish inspection after observing demand; drop preserves a retry"]
pub(crate) struct PartitionInspection<'a, S> {
    dispatch: &'a PartitionDispatch<S>,
    source: Option<Arc<PartitionSource<S>>>,
    detachment: Arc<SourceDetachment>,
}

impl<S> PartitionDispatch<S> {
    /// Final demand observation and worker parking must exclude source publication.
    /// The private caller may take the worker-state lock, but must not call any
    /// scheduler/provider or reenter this dispatcher while this closure runs.
    pub(crate) fn with_demand<R>(&self, inspect: impl FnOnce(bool) -> R) -> R {
        let sources = self.sources.lock();
        inspect(!sources.is_empty())
    }

    /// Prepare service storage outside IRQ/spinlocked paths.
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            sources: SpinNoIrq::new(LinkedList::new(SourceAdapter::new())),
        })
    }

    /// Prepare a detached source before publishing its notification capability.
    /// Allocates source/control storage; notification and inspection do not allocate.
    pub fn create_source(self: &Arc<Self>, payload: S) -> Arc<PartitionSource<S>> {
        Arc::new(PartitionSource {
            owner: Arc::downgrade(self),
            links: AtomicLink::new(),
            state: UnsafeCell::new(PartitionDispatchState::new()),
            detachment: SourceDetachment::new(),
            payload,
        })
    }

    /// Retain a request in IRQ, BH or task context without calling a provider.
    /// A source belongs to exactly this service; a mismatch is an internal bug.
    ///
    /// # Panics
    /// Panics if source belongs to another dispatcher.
    pub fn notify(&self, source: &Arc<PartitionSource<S>>) {
        self.assert_owns(source);
        let mut sources = self.sources.lock();
        // SAFETY: The Weak identity pins the owner's allocation against reuse;
        // the check above proves this is its lock. No state reference escapes.
        if unsafe { &mut *source.state.get() }.notify() {
            sources.push_back(source.clone());
        }
    }

    /// Validate a prepared capability before exposing it to a producer.
    pub(crate) fn assert_owns(&self, source: &Arc<PartitionSource<S>>) {
        assert!(
            core::ptr::eq(source.owner.as_ptr(), self),
            "foreign partition source"
        );
    }

    /// Permanently stop selecting this source; late notifications become no-ops.
    /// The source's prepared durable receipt completes after selected inspection has
    /// returned its source reference. It does NOT wait for acquired callbacks or
    /// for references still held by the publisher, and does not stop the service.
    ///
    /// Caller must arrange admission closure and accepted-work drain before using
    /// detach for queue shutdown: detaching pending demand is not cancellation.
    /// This operation never waits or allocates. Register/recheck the receipt using
    /// the ordinary Completion protocol; wait only in a suitable task context.
    ///
    /// # Panics
    /// Panics for a foreign source or inconsistent internal membership/accounting.
    pub fn detach(&self, source: &Arc<PartitionSource<S>>) {
        self.assert_owns(source);
        let mut sources = self.sources.lock();
        // SAFETY: The immutable Weak owner identifies exactly this lock. State
        // access stays inside the critical section shared with notify/selection.
        let (remove_node, close_accounting) = unsafe { &mut *source.state.get() }.detach();
        let removed = if remove_node {
            // SAFETY: Queued -> Detached under the owner's lock proves this exact
            // node is on this list. The Arc keeps it alive and the lock excludes
            // selection/removal until this O(1) unlink completes.
            let mut cursor = unsafe { sources.cursor_mut_from_ptr(Arc::as_ptr(source)) };
            Some(cursor.remove().expect("queued source must be linked"))
        } else {
            None
        };
        drop(sources);
        drop(removed);
        if close_accounting {
            source.detachment.mark_detached();
        }
    }

    /// Remove one source in O(1). Inspect management only after this returns.
    pub(crate) fn begin_inspection(&self) -> Option<PartitionInspection<'_, S>> {
        let mut sources = self.sources.lock();
        let source = sources.pop_front()?;
        // SAFETY: Only notify/finish insert, both into the node's unique owner.
        // Its service lock covers removal and this transition together.
        unsafe { &mut *source.state.get() }.begin_inspection();
        let detachment = source.detachment.clone();
        detachment.begin_inspection();
        drop(sources);
        Some(PartitionInspection {
            dispatch: self,
            source: Some(source),
            detachment,
        })
    }
}

impl<S> PartitionInspection<'_, S> {
    /// Borrow the adapter with no service lock held. Not a callback-run API.
    pub(crate) fn source(&self) -> &S {
        &self.source.as_ref().unwrap().payload
    }

    /// End this turn before invoking acquired work; retain concurrent notices
    /// even when management observed no remaining work.
    pub(crate) fn finish(mut self, remainder: SourceRemainder) {
        self.return_source(remainder, || {});
    }

    #[cfg(unittest)]
    pub(crate) fn finish_with_release_hook(
        mut self,
        remainder: SourceRemainder,
        hook: impl FnOnce(),
    ) {
        self.return_source(remainder, hook);
    }

    fn return_source(&mut self, remainder: SourceRemainder, before_release: impl FnOnce()) {
        let Some(source) = self.source.take() else {
            return;
        };
        let mut sources = self.dispatch.sources.lock();
        // SAFETY: This unique turn was created only by this dispatcher's locked
        // removal. Its lifetime retains the service; all state access uses this
        // same lock, including notifications while inspection was unlocked.
        let needs_requeue = unsafe { &mut *source.state.get() }.finish_inspection(remainder);
        let retired = if needs_requeue {
            sources.push_back(source);
            None
        } else {
            Some(source)
        };
        drop(sources);
        before_release();
        // This independent control survives final source/payload destruction.
        drop(retired);
        self.detachment.finish_inspection();
    }
}

impl<S: PartitionWorkSource> PartitionDispatch<S> {
    /// Service at most `budget` source turns, including empty acquisitions.
    /// Uses the same CPU/category and unlocked-caller contract as run_one_turn.
    /// A BH provider must additionally serialize its local drainer.
    ///
    /// Returns whether listed demand remains at the final observation. True
    /// asks the provider for another opportunity, outside all dispatch locks.
    /// False is NOT permission to park: producers must independently request
    /// service for notifications racing this observation. No wake is issued here.
    /// The bound limits turns, not callback duration or wall-clock execution time.
    ///
    /// # Panics
    /// Propagates adapter, callback, cleanup and Waker panics; also panics on
    /// violated internal dispatch or inspection-accounting invariants.
    pub fn run_bounded_pass(&self, budget: core::num::NonZeroUsize) -> bool {
        for _ in 0..budget.get() {
            if !self.run_one_turn() {
                return false;
            }
        }
        !self.sources.lock().is_empty()
    }

    /// Inspect at most one source and consume its acquired execution, if any.
    /// Caller already owns the matching CPU/category execution context, with no
    /// conflicting locks held. This method does not establish runtime context.
    ///
    /// True means a SOURCE TURN was serviced, not that a callback ran. An empty
    /// source still consumes a turn so pass budgeting also bounds empty scans.
    /// False means only that the source list was empty at selection, not that it
    /// is safe to park or that all previously acquired work has finished.
    ///
    /// # Panics
    /// Propagates adapter, callback, cleanup and Waker panics; also panics on
    /// violated internal dispatch or inspection-accounting invariants.
    pub fn run_one_turn(&self) -> bool {
        let Some(inspection) = self.begin_inspection() else {
            return false;
        };
        let (execution, remainder) = inspection.source().acquire();
        inspection.finish(remainder);
        if let Some(execution) = execution {
            execution.run();
        }
        true
    }
}

impl<S> Drop for PartitionInspection<'_, S> {
    fn drop(&mut self) {
        self.return_source(SourceRemainder::More, || {});
    }
}
