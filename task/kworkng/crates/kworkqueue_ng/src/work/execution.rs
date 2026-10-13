// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Non-owning execution, consumed once after the provider finishes source inspection.

use alloc::sync::Arc;
use core::{ptr::NonNull, task::Waker};

use kworkerpool_ng::AcquiredExecution;

use super::{Work, state::WorkSlot};

/// Internal authority to run and retire the occurrence already claimed from Work.
///
/// This capability borrows stable caller-owned Work; it does not allocate or
/// retain ownership of that Work. This opaque type is an execution-provider
/// contract, not a caller-facing guard. Providers must consume it
/// with run; dropping/forgetting it does
/// not cancel execution, complete observers or make another claim possible.
/// The unsafe publisher must keep Work and all captures valid through the final
/// access. Its lifetime is NOT derived from a temporary Queue/source borrow.
#[must_use = "a claimed execution must be run to completion"]
pub struct Execution {
    work: NonNull<()>,
    run: unsafe fn(NonNull<()>),
    #[cfg(unittest)]
    finish: unsafe fn(NonNull<()>),
}

/// Test-only typed acquisition keeps the safe, unpartitioned fixture borrowed.
#[cfg(unittest)]
pub(crate) struct BorrowedExecution<'a> {
    execution: Execution,
    _borrow: core::marker::PhantomData<&'a ()>,
}

#[cfg(unittest)]
impl BorrowedExecution<'_> {
    pub(crate) fn run(self) {
        self.execution.run();
    }

    pub(crate) fn run_with(self, callback: impl FnOnce()) {
        self.execution.run_with(callback);
    }
}

impl<T: Send, H: Fn() + Send + Sync> Work<T, H> {
    /// Claims an explicit test target before returning the exclusive capability.
    ///
    /// Returns None for idle, running, unreleased waiting work or a changed route.
    /// The partition revalidates ownership and consumes the ready reservation
    /// without charging twice. Production uses Queue::claim_with_demand; the
    /// unpartitioned path serves local lifecycle unit tests.
    #[cfg(unittest)]
    pub(crate) fn claim(&self) -> Option<BorrowedExecution<'_>> {
        loop {
            let partition = self.state.lock().pending_partition();
            if let Some(partition) = partition {
                return partition.claim(self);
            }
            let mut state = self.state.lock();
            if state.has_partitioned_pending() {
                continue;
            }
            if !state.claim() {
                return None;
            }
            return Some(Execution::borrowed(self));
        }
    }
}

impl Execution {
    #[cfg(unittest)]
    pub(crate) fn borrowed<T: Send, H: Fn() + Send + Sync>(
        work: &Work<T, H>,
    ) -> BorrowedExecution<'_> {
        BorrowedExecution {
            // SAFETY: the typed caller owns the unique running occurrence; this
            // fixture borrows its concrete Work through consumption.
            execution: unsafe { Self::from_ptr(NonNull::from(work)) },
            _borrow: core::marker::PhantomData,
        }
    }

    /// Binds a concrete claimed Work without requiring its captures to be static.
    ///
    /// # Safety
    /// work must be shared, initialized and stable until this unique execution is
    /// consumed. Its running slot must already be claimed. Callers deriving it from
    /// a queue rely on the unsafe publication contract, not the queue borrow alone.
    pub(super) unsafe fn from_ptr<T: Send, H: Fn() + Send + Sync>(
        work: NonNull<Work<T, H>>,
    ) -> Self {
        Self {
            work: work.cast(),
            run: Work::<T, H>::run_claimed,
            #[cfg(unittest)]
            finish: Work::<T, H>::finish_claimed,
        }
    }

    /// Runs Work's permanently bound callback and retires this occurrence once.
    ///
    /// End source inspection and release provider/partition locks before calling.
    /// The callback and resource destruction run without Work protection. Running
    /// stays occupied through both. Copy all terminal metadata before clearing
    /// running: a later cancel_sync may observe idle and permit storage release
    /// without waiting for completion wakes. Callback captures remain owned by
    /// Work across completion/cancellation;
    /// only accepted occurrence resources are released here. Execution-context
    /// validation belongs to the bound execution provider. The callback must return normally;
    /// recovery is not supplied.
    ///
    /// # Panics
    /// Propagates callback, resource destructor or Waker panics. A callback or
    /// destructor panic does not publish successful completion. Also panics if
    /// internal code violated the unique running-slot ownership invariant.
    pub(crate) fn run(self) {
        // SAFETY: construction paired the pointer with its concrete function.
        // This single-use capability owns the claim; storage stays live through
        // its final access, and no pointer access follows terminal notification.
        unsafe { (self.run)(self.work) };
    }

    /// Injects a callback phase for existing lifecycle/reentry regression tests only.
    ///
    /// # Panics
    /// Propagates injected callback, resource destructor and notification panics;
    /// also panics if the claimed running-slot invariant is violated.
    #[cfg(unittest)]
    pub(crate) fn run_with(self, callback: impl FnOnce()) {
        callback();
        // SAFETY: same unique claim and concrete pointer pairing as run; only the
        // test callback phase differs, not storage lifetime or terminal cleanup.
        unsafe { (self.finish)(self.work) };
    }
}

impl AcquiredExecution for Execution {
    fn run(self) {
        Execution::run(self);
    }
}

impl<T: Send, H: Fn() + Send + Sync> Work<T, H> {
    /// # Safety
    /// work must identify this concrete type's unique, live claimed occurrence.
    unsafe fn run_claimed(work: NonNull<()>) {
        let (finished, retirement) = {
            // SAFETY: only Execution::from_ptr installs this function, paired with
            // the same concrete Work type and its stable, uniquely claimed storage.
            let work = unsafe { work.cast::<Self>().as_ref() };
            #[cfg(feature = "self-wait-check")]
            {
                let partition = work.state.lock().running_partition();
                // SAFETY: the unique claim and publication contract keep Work and
                // its embedded node stable/shared/live. This is its original running
                // partition. The callback must return; unlink precedes Work cleanup.
                unsafe { work.callback_node.run(partition, || (work.callback)()) };
            }
            #[cfg(not(feature = "self-wait-check"))]
            (work.callback)();
            // Clearing running permits a new cancel_sync to return immediately.
            // It must not leave a subsequent read from externally owned Work.
            let retirement = work.retirement;
            (work.finish_execution(), retirement)
        };
        Self::complete_execution(finished);
        if let Some(retirement) = retirement {
            retirement.run();
        }
    }

    #[cfg(unittest)]
    /// # Safety
    /// work must identify this concrete type's unique, live claimed occurrence.
    unsafe fn finish_claimed(work: NonNull<()>) {
        let (finished, retirement) = {
            // SAFETY: same unique live execution as the enclosing test entry.
            let work = unsafe { work.cast::<Self>().as_ref() };
            // Keep the same pre-retirement snapshot as the production path.
            let retirement = work.retirement;
            (work.finish_execution(), retirement)
        };
        Self::complete_execution(finished);
        if let Some(retirement) = retirement {
            retirement.run();
        }
    }

    // This consumes the running claim's storage protection. Callers must copy
    // every field they still need before entering, and never access Work after.
    fn finish_execution(&self) -> (WorkSlot<Option<T>>, Option<Arc<Waker>>) {
        let resources = self
            .state
            .lock()
            .take_running_resources()
            .expect("claimed execution owns running resources");
        drop(resources);
        {
            let partition = self.state.lock().running_partition();
            if let Some(partition) = partition {
                partition.finish(self)
            } else {
                (
                    self.state
                        .lock()
                        .finish_after_cleanup()
                        .expect("execution cleanup finished before retirement"),
                    None,
                )
            }
        }
    }

    // No Work borrow may cross notification: waking a waiter may free its owner,
    // even if a still-active &self function would make no further source accesses.
    fn complete_execution((finished, kick): (WorkSlot<Option<T>>, Option<Arc<Waker>>)) {
        if let Some(kick) = kick {
            kick.wake_by_ref();
        }
        finished.complete();
    }
}

#[cfg(unittest)]
mod tests;
