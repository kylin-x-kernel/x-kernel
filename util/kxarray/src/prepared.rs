// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private capacity owned independently of any live radix path.

use crate::{
    Allocator, EntryOwner,
    node::{MAX_HEIGHT, PreparedNodes},
    retirement::Retirement,
};

/// Private retirement storage for one removal, not an RCU admission permit.
///
/// Created by [`crate::XArray::prepare_remove`]. It neither reserves a key nor
/// retains an entry; dropping it only cancels unpublished storage. It cannot
/// be cloned and is consumed by [`crate::XArray::remove_prepared`] or
/// [`crate::XArray::withdraw_prepared`]. Allocation
/// and cancellation do not require an RCU service, but do require a context
/// supporting allocator operations. The ordinary removal entrance validates
/// current context; the advanced withdrawal entrance requires caller preflight.
///
/// A removal consumes its record even when the key is absent:
///
/// ```compile_fail,E0382
/// /// # Safety
/// /// Run in admitted task context with the permanent RCU service available,
/// /// without RCU scopes or scheduler/bookkeeping locks. Stored owners must
/// /// permit nonblocking, non-unwinding release in callback context.
/// unsafe fn twice(array: &kxarray::XArray<std::sync::Arc<usize>>,
///          token: kxarray::PreparedRemove<std::sync::Arc<usize>>) {
///     // SAFETY: the caller supplies the removal context and release contract;
///     // remove_prepared rechecks preflight before consuming this fresh record.
///     unsafe { array.remove_prepared(0, token) }.unwrap();
///     // SAFETY: the same context/release obligations apply. This deliberately
///     // violates single-use ownership: the compiler must reject the moved token.
///     unsafe { array.remove_prepared(1, token) }.unwrap();
/// }
/// ```
pub struct PreparedRemove<P: EntryOwner> {
    pub(super) record: kalloc_types::KBox<Retirement<P>>,
}

/// Withdrawn ownership awaiting RCU submission after caller-held locks are released.
///
/// Created only by [`crate::XArray::withdraw_prepared`]. The association is
/// already absent, but old readers may still use its payload and detached nodes.
/// This object grants neither payload access nor permission to free them.
/// It is task-local and cannot be cloned. Dropping or forgetting it leaks the
/// withdrawn storage rather than freeing data still reachable by readers;
/// dropping releases only the unused callback record. Complete normal paths
/// with [`Self::retire`], including when a later operation fails.
///
/// Pending retirement cannot be transferred to a different task:
///
/// ```compile_fail
/// fn require_send<T: Send>() {}
/// require_send::<kxarray::PendingRetirement<std::sync::Arc<usize>>>();
/// ```
///
/// Submission consumes the only retirement right:
///
/// ```compile_fail,E0382
/// /// # Safety
/// /// Remain on the withdrawing task after restoring its preflight context:
/// /// release writer/scheduler/RCU bookkeeping locks, hold no read scope, and
/// /// keep the permanent service and participants available. No arbitrary code
/// /// or explicit context transition may intervene before submission.
/// unsafe fn twice(pending: kxarray::PendingRetirement<std::sync::Arc<usize>>) {
///     // SAFETY: pending owns the single withdrawal right, and the caller
///     // guarantees restored submission context on the withdrawing task.
///     unsafe { pending.retire() };
///     // SAFETY: submission context is unchanged. This intentionally reuses a
///     // consumed retirement right, which the compiler must reject.
///     unsafe { pending.retire() };
/// }
/// ```
#[must_use = "withdrawn storage must be retired after releasing caller-held locks"]
pub struct PendingRetirement<P: EntryOwner> {
    pub(super) record: kalloc_types::KBox<Retirement<P>>,
    pub(super) owner: *mut (),
    pub(super) detached: [*mut crate::node::Node; MAX_HEIGHT as usize],
}

impl<P: EntryOwner + 'static> PendingRetirement<P> {
    /// Submits the withdrawn payload and empty nodes without allocating or waiting.
    ///
    /// This consumes the retirement right. It does not wait for readers or
    /// callback execution. An unexpected submission rejection is a fatal
    /// invariant violation, not a recoverable unchanged-publication error.
    ///
    /// # Safety
    /// The task that performed withdrawal must first release all writer,
    /// scheduler and RCU bookkeeping locks and restore its preflight context.
    /// Hold no RCU scope. Keep the permanent RCU service and participating CPUs
    /// available, with no arbitrary caller code or explicit execution-context
    /// transition between withdrawal and submission. Involuntary preemption
    /// and migration within the participating set remain permitted.
    pub unsafe fn retire(self) {
        // SAFETY: withdraw_prepared transferred exactly one displaced owner
        // and uniquely detached empty nodes. This call's contract restores the
        // admitted task context and excludes locks needed by submission.
        unsafe { Retirement::<P>::submit_with_nodes(self.record, self.owner, self.detached) };
    }
}

impl<P: EntryOwner> PreparedRemove<P> {
    pub(super) fn with_allocator(allocator: &mut impl Allocator) -> kerrno::KResult<Self> {
        Ok(Self {
            record: Retirement::prepare(allocator)?,
        })
    }
}

/// Single-use insertion capacity for one fixed key, not a reservation of that key.
///
/// Created by [`crate::XArray::prepare_insert`]. It owns only unpublished nodes,
/// can be used with another array, and can be cancelled by dropping it without
/// an RCU service. Drop requires a context supporting allocator deallocation,
/// not NMI. No payload or borrowed tree path is retained.
///
/// A committed token cannot be reused:
///
/// ```compile_fail,E0382
/// /// # Safety
/// /// Run in initialized task/early-boot context, not IRQ/NMI/BH or an RCU
/// /// scope, with allocator access for Arc creation and private-node release.
/// /// Arc<usize> release must be nonblocking and non-unwinding on this platform.
/// unsafe fn twice(array: &kxarray::XArray<std::sync::Arc<usize>>, token: kxarray::PreparedInsert) {
///     // SAFETY: the caller establishes the context and release requirements;
///     // this first insertion consumes the fresh, key-bound capacity token.
///     unsafe { array.insert_prepared(std::sync::Arc::new(1), token) }.unwrap();
///     // SAFETY: context and payload release requirements are unchanged. This
///     // deliberate reuse of consumed capacity must fail at compile time.
///     unsafe { array.insert_prepared(std::sync::Arc::new(2), token) }.unwrap();
/// }
/// ```
pub struct PreparedInsert {
    pub(super) index: usize,
    pub(super) nodes: PreparedNodes,
}

impl PreparedInsert {
    pub(super) fn with_allocator(
        index: usize,
        allocator: &mut impl Allocator,
    ) -> kerrno::KResult<Self> {
        let mut nodes = PreparedNodes::new();
        // Empty roots need H nodes; growth needs H-R + H-1; descent under an
        // existing root needs at most R-1. H,R <= MAX_HEIGHT, so this bound
        // remains sufficient even if another writer changes the root meanwhile.
        nodes.prepare(2 * MAX_HEIGHT as usize, allocator)?;
        Ok(Self { index, nodes })
    }
}
