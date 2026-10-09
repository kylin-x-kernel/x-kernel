// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Handle ownership and prepared task-context reclamation, independent of admission.

use alloc::sync::Arc;
use core::{cell::UnsafeCell, mem::ManuallyDrop, pin::Pin};

use kspin::SpinNoIrq;

use crate::{DefaultQueue, DelayedWork, SubmitError, WaitError, Work, WorkQueue, services};

/// Owned Work preparation failed before creating a usable handle.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HandleCreateError {
    /// Preparation requires sleepable task context.
    InvalidContext,
    /// The permanent normal service is not ready for eventual reclamation.
    ServicesUnavailable,
}

pub(super) trait Drain: Send + Sync + 'static {
    fn drain(&self) -> Result<bool, WaitError>;
}

impl<H: Fn() + Send + Sync + 'static> Drain for Work<H> {
    fn drain(&self) -> Result<bool, WaitError> {
        self.cancel_sync()
    }
}

impl<H: Fn() + Send + Sync + 'static> Drain for DelayedWork<H> {
    fn drain(&self) -> Result<bool, WaitError> {
        self.cancel_sync()
    }
}

struct Storage<W> {
    // Reclamation drops W in place in task context; the final allocation release
    // may instead occur on the publishing CPU if reclamation wins that race.
    work: UnsafeCell<ManuallyDrop<W>>,
    reclaim: Work<fn()>,
}

// SAFETY: W is shared only while handles or its asynchronous callbacks exist.
// Last-handle Drop prevents further API calls; reclamation drains callbacks
// before exclusively destroying W. The independent reclaim field stays live.
unsafe impl<W: Drain> Sync for Storage<W> {}

pub(super) struct Owner<W: Drain> {
    storage: Arc<Storage<W>>,
    // Only new() selects this permanent normal default; never replace it with
    // a private or BH queue, since terminal reclamation may block.
    queue: Arc<WorkQueue>,
    is_closed: SpinNoIrq<bool>,
}

impl<W: Drain> Owner<W> {
    pub(super) fn new(prepare: impl FnOnce() -> W) -> Result<Arc<Self>, HandleCreateError> {
        if !ktask::is_sleepable_context() {
            return Err(HandleCreateError::InvalidContext);
        }
        let queue = services::with_default_queue(DefaultQueue::Normal, |queue| queue.cloned())
            .ok_or(HandleCreateError::ServicesUnavailable)?;
        let work = prepare();
        let storage = Arc::new_cyclic(|weak: &alloc::sync::Weak<Storage<W>>| Storage {
            work: UnsafeCell::new(ManuallyDrop::new(work)),
            // SAFETY: the weak address becomes the stable Arc allocation before
            // exposure. Owner::drop submits exactly once, transferring one strong
            // reference to reclaim_storage. No reclaim Work address is exported.
            // The selected permanent normal queue allows both the callback and
            // the retirement hook's synchronous drain to run in task context.
            reclaim: unsafe {
                kworkqueue_ng::service::prepare_retiring_work(
                    weak.as_ptr().cast(),
                    Self::reclaim_storage,
                )
            },
        });
        Ok(Arc::new(Self {
            storage,
            queue,
            is_closed: SpinNoIrq::new(false),
        }))
    }

    pub(super) fn queue(&self) -> &Arc<WorkQueue> {
        &self.queue
    }

    pub(super) fn work(&self) -> Pin<&W> {
        // SAFETY: a live Owner borrow excludes last-handle Drop and reclamation.
        // The private Arc storage is stable and no mutable W access is exposed.
        let work = unsafe { &*self.storage.work.get().cast::<W>() };
        // SAFETY: Private Arc storage cannot move W and exposes no mutable W
        // access; this Owner borrow prevents final-handle reclamation.
        unsafe { Pin::new_unchecked(work) }
    }

    pub(super) fn submit<R>(&self, submit: impl FnOnce(Pin<&W>) -> R) -> Result<R, SubmitError> {
        let closed = self.is_closed.lock();
        if *closed {
            return Err(SubmitError::Closed);
        }
        // Keep the gate until admission returns. Shutdown closes it before drain;
        // no pre-closure submitter can publish after that drain starts.
        Ok(submit(self.work()))
    }

    pub(super) fn shutdown(&self) -> Result<(), WaitError> {
        if !ktask::is_sleepable_context() {
            return Err(WaitError::InvalidContext);
        }
        *self.is_closed.lock() = true;
        self.work().drain().map(|_| ())
    }

    /// # Safety
    /// context owns the single raw Arc transferred by this Owner's final Drop.
    /// The private reclaim Work has retired and no public Owner reference exists.
    unsafe fn reclaim_storage(context: *const ()) {
        // SAFETY: one final Owner Drop transfers exactly this raw Arc; the
        // terminal hook is invoked once and no other path reconstructs it.
        let storage = unsafe { Arc::from_raw(context.cast::<Storage<W>>()) };
        {
            // SAFETY: W has not yet been destroyed; all producers have lost their
            // handles. Drain ends both callbacks and, for delayed Work, timer tails.
            let work = unsafe { &*storage.work.get().cast::<W>() };
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Reclamation runs on the normal \
                          worker outside the reclaimed callback; fresh completion observers have \
                          inline registration capacity"
            )]
            work.drain()
                .expect("normal reclamation can drain owned work");
        }
        // SAFETY: no handles/API borrows remain and successful drain ended all
        // asynchronous W access. UnsafeCell permits destruction of this field
        // while the publishing CPU may still borrow the separate reclaim field.
        let work = unsafe { &mut *storage.work.get() };
        // SAFETY: This final-Owner retirement is the sole destruction of W;
        // successful drain ended all asynchronous work accesses.
        unsafe { ManuallyDrop::drop(work) };
        // Storage's remaining fields contain no user captures. Arc keeps the
        // reclaim node allocated until its publisher has also returned.
    }
}

impl<W: Drain> Drop for Owner<W> {
    fn drop(&mut self) {
        let context = Arc::into_raw(self.storage.clone());
        // SAFETY: the transferred Arc keeps storage alive until the terminal
        // hook. self.storage additionally retains the reclaim node through this
        // publication call even if remote execution/retirement finishes first.
        // self.queue is the permanent normal default chosen exclusively by new(),
        // so the retirement hook's cancel_sync/drain may sleep in worker context.
        // This node is private, submitted once and never independently canceled.
        let reclaim = unsafe { &(*context).reclaim };
        // SAFETY: The transferred Arc and self.storage keep this private node
        // at a stable address; no mutable access can move it after publication.
        let reclaim = unsafe { Pin::new_unchecked(reclaim) };
        // SAFETY: The raw Arc survives until the terminal hook and self.storage
        // keeps the node live through publication. The permanent normal queue
        // permits the retirement hook's sleeping drain, and this is its sole submit.
        let result = unsafe { self.queue.queue_work(reclaim, None) };
        // Failure cannot free W prematurely: the raw strong reference remains.
        // The permanent default queue cannot close; other failures are fatal
        // invariant/exhaustion conditions, consistent with kernel OOM policy.
        assert!(
            result.is_ok(),
            "owned work reclamation admission failed: {result:?}"
        );
    }
}
