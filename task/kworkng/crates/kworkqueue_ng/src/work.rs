// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private Work lifecycle operations and their lock-protected state.

mod cancellation;
pub(crate) mod delayed;
pub(crate) mod disable;
pub(crate) mod execution;
pub(crate) mod observers;
pub(crate) mod state;
pub(crate) mod wait;

use alloc::{sync::Arc, vec::Vec};
use core::{mem::offset_of, ptr::NonNull};

use kspin::SpinNoIrq;

use self::{execution::Execution, observers::PreparedObserver, state::WorkState};
use crate::queue::partition::{QueuePartition, pending::PendingNode};

/// Owns the prepared Work lifecycle and its protected pending/running state.
///
/// The callback and its captures belong to Work, not individual occurrences.
/// Public callers use the fixed-payload crate-level Work alias.
/// The embedded pending node is borrowed
/// by its partition; admission requires stable externally retained Work storage.
/// Partition protection precedes Work protection.
pub struct Work<T, H = fn()> {
    callback: H,
    retirement: Option<Retirement>,
    pending_node: PendingNode,
    #[cfg(feature = "self-wait-check")]
    callback_node: crate::queue::partition::callbacks::CallbackNode,
    pub(crate) state: SpinNoIrq<WorkState<T>>,
}

/// Provider-owned terminal action, copied out before the last Work borrow ends.
#[derive(Clone, Copy)]
pub(crate) struct Retirement {
    pub(crate) context: *const (),
    pub(crate) call: unsafe fn(*const ()),
}

// SAFETY: only the unsafe provider constructor installs this immutable pair;
// its contract requires cross-CPU validity through terminal invocation.
unsafe impl Send for Retirement {}
// SAFETY: no shared mutation is possible; invocation obeys the constructor's
// exclusive publication/retirement contract, not arbitrary shared access.
unsafe impl Sync for Retirement {}

impl Retirement {
    pub(super) fn run(self) {
        // SAFETY: provider construction pairs this function and retained context.
        // Execution calls it once after all Work borrows and completion accesses.
        unsafe { (self.call)(self.context) };
    }
}

impl<T: Send> Work<T> {
    /// Builds a no-op Work for lifecycle-only unit tests.
    #[cfg(unittest)]
    pub(crate) fn new() -> Self {
        Self::with_callback(|| {})
    }
}

impl<T, H> Work<T, H> {
    /// Binds one callback and its captures for the entire externally owned lifetime.
    ///
    /// Construction does not allocate, publish an address or invoke the callback.
    /// Moving Work is allowed before publication; later admission requires stable
    /// storage. Occurrence cleanup never drops these captures.
    pub fn with_callback(callback: H) -> Self
    where
        T: Send,
        H: Fn() + Send + Sync,
    {
        Self {
            callback,
            retirement: None,
            #[cfg(feature = "self-wait-check")]
            callback_node: crate::queue::partition::callbacks::CallbackNode::new(),
            pending_node: PendingNode::with_claim(
                Self::claim_pending_node,
                Self::release_pending_node,
            ),
            state: SpinNoIrq::new(WorkState::new()),
        }
    }

    pub(crate) fn set_retirement(&mut self, retirement: Retirement) {
        self.retirement = Some(retirement);
    }

    /// Borrows the link without allowing replacement of its concrete claim entry.
    pub(crate) fn pending_node(&self) -> &PendingNode {
        &self.pending_node
    }

    /// Preserves the enclosing allocation's pointer provenance for later projection.
    pub(crate) fn pending_ptr(&self) -> NonNull<PendingNode> {
        let work = NonNull::from(self);
        // SAFETY: offset_of names an initialized in-bounds field of this Work.
        // Derive from the whole Work pointer, not a narrowed field reference.
        unsafe { work.byte_add(offset_of!(Self, pending_node)).cast() }
    }

    /// Claims the concrete Work represented by this node under partition protection.
    ///
    /// # Safety
    /// node must originate from pending_ptr on this exact Work<T, H> type. Its
    /// allocation must remain shared, stable and live through returned execution.
    /// The caller must hold this partition's state lock through ready unlink and
    /// claim, and must consume any returned execution once.
    unsafe fn claim_pending_node(
        node: NonNull<PendingNode>,
        partition: &Arc<QueuePartition>,
    ) -> Option<Execution>
    where
        T: Send,
        H: Fn() + Send + Sync,
    {
        // SAFETY: the matching constructor binds this function to its concrete
        // Work type; pending_ptr preserved full-allocation provenance and the
        // caller retains storage through both pending inspection and execution.
        let work_ptr = unsafe { node.byte_sub(offset_of!(Self, pending_node)).cast::<Self>() };
        // SAFETY: the reconstructed full Work pointer is valid for this protected
        // inspection; no reference lifetime is extended beyond the local borrow.
        let work = unsafe { work_ptr.as_ref() };
        let mut state = work.state.lock();
        assert!(
            state.pending_on(partition),
            "linked Work belongs to inspected partition"
        );
        if !state.claim() {
            return None;
        }
        // SAFETY: claim just moved the occurrence to running under its partition
        // guard. Admission keeps concrete storage/captures live through consumption.
        Some(unsafe { Execution::from_ptr(work_ptr) })
    }

    /// Marks a waiting occurrence ready under the owning partition's allowance.
    ///
    /// # Safety
    /// node must be this concrete Work's pending_ptr, live/shared throughout the
    /// call. Hold its owning partition guard to exclude removal and owner changes.
    unsafe fn release_pending_node(
        node: NonNull<PendingNode>,
        partition: &Arc<QueuePartition>,
    ) -> bool {
        // SAFETY: the constructor pairs this entry with its concrete layout and
        // pending_ptr retains enclosing provenance; the partition retains access.
        let work_ptr = unsafe { node.byte_sub(offset_of!(Self, pending_node)).cast::<Self>() };
        // SAFETY: caller retains the full shared Work during this protected check.
        let mut state = unsafe { work_ptr.as_ref() }.state.lock();
        assert!(
            state.pending_on(partition),
            "linked Work belongs to inspected partition"
        );
        state.release_pending()
    }

    /// Accepts prepared resources; rejection returns them for unlocked disposal.
    ///
    /// # Errors
    /// Returns the original payload if a pending occurrence already exists.
    #[cfg(unittest)]
    pub(crate) fn accept(&self, resources: T) -> Result<(), T> {
        self.state.lock().accept(resources)
    }

    /// Samples preparation capacity without exporting any subject address.
    pub(crate) fn observer_count(&self) -> usize {
        self.state.lock().observer_count()
    }

    /// Attaches one prepared observer per current subject in one transaction.
    ///
    /// Prepare receipt storage outside the lock. On success,
    /// exactly the returned number of tokens are consumed from the vector end.
    /// Extra tokens remain caller-owned. No resources or observers are destroyed.
    ///
    /// # Errors
    /// Returns the required capacity without attaching anything if preparation
    /// was insufficient. The caller may prepare more and retry; no subject
    /// pointer or earlier count is assumed valid across this gap.
    pub(crate) fn capture(&self, prepared: &mut Vec<PreparedObserver>) -> Result<usize, usize> {
        let mut state = self.state.lock();
        let needed = state.observer_count();
        if prepared.len() < needed {
            return Err(needed);
        }
        state.capture(prepared);
        Ok(needed)
    }
}
