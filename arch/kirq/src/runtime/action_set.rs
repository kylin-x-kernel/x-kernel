// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Line-serialized action chain with admission-protected borrowed traversal.
//!
//! Unlike intrusive_collections::AtomicLink, withdrawal preserves the detached
//! forward link. Ordinary readers can follow it until the original Line drains;
//! PerCpu removal instead requires caller-completed execution exclusion.
//! No action grace period or per-action reader count is required.

use core::{
    marker::PhantomData,
    ptr,
    sync::atomic::{AtomicPtr, Ordering},
};

use kalloc_types::KBox;

use super::action::{IrqAction, IrqActionToken, PreparedAction};

/// Stable allocation published into one Line's chain.
/// Payload and sequence become immutable at publication; only the atomic
/// successor changes, and unlink deliberately leaves the detached link intact.
pub(super) struct ActionRecord {
    action: IrqAction,
    sequence: usize,
    next: AtomicPtr<ActionRecord>,
}

impl ActionRecord {
    pub(super) fn set_user_affinity_restricted(&mut self) {
        self.action.set_user_affinity_restricted();
    }

    pub(super) fn new(action: IrqAction) -> Self {
        Self {
            action,
            sequence: 0,
            next: AtomicPtr::new(ptr::null_mut()),
        }
    }
}

/// Unique detached allocation; abandonment deliberately leaks rather than
/// destroying an owner that an admitted reader might still be borrowing.
#[must_use = "exclude all old readers before reclaiming outside locks"]
pub(super) struct RetiredAction {
    raw: *mut ActionRecord,
}

// SAFETY: Handler is Arc<dyn IrqHandler> (backend/platform.rs), whose trait
// requires Send + Sync (model/desc.rs). The remaining ActionRecord fields are
// immutable scalars and an atomic link. KBox permits cross-CPU deallocation for
// Send payloads (kalloc-types/src/kbox.rs); this raw right preserves that unique
// ownership. Moving it neither accesses the node nor frees it on Drop, and
// reclaim independently requires the end of every reader's borrow.
unsafe impl Send for RetiredAction {}

impl RetiredAction {
    /// Recovers the detached action and releases only its node allocation.
    ///
    /// # Safety
    /// All admissions and snapshots that could reach this node must have ended,
    /// including snapshots captured before unlink whose traversal began later.
    /// No future reader may reach it. For ordinary actions, observe the original
    /// Line's drain under its lock while management excludes competing membership
    /// mutations. For PerCpu actions, the source owner must have completed every
    /// admitted execution and original completion and excluded restart.
    /// A private fixture may instead prove that all of its snapshots ended and
    /// that no other reader exists.
    /// Call outside short locks; drop the returned action outside management.
    pub(super) unsafe fn reclaim(self) -> IrqAction {
        // SAFETY: withdrawal transferred the unique KBox allocation into this
        // right; the caller excludes every remaining borrow and future access.
        let record = unsafe { KBox::from_raw(self.raw) };
        KBox::into_inner(record).action
    }
}

/// Borrowed traversal bounded by the publication sequence at capture.
/// It owns no node and performs no drain: the capturing execution protects
/// every reachable allocation until traversal ends.
pub(super) struct IrqActionSnapshot<'a> {
    head: *mut ActionRecord,
    ceiling: usize,
    admission: PhantomData<&'a ActionRecord>,
}

impl IrqActionSnapshot<'_> {
    pub(super) fn with_actions(self, mut callback: impl FnMut(&IrqAction)) {
        let mut raw = self.head;
        while !raw.is_null() {
            // SAFETY: snapshot's admission contract keeps every reachable node
            // alive through this consuming traversal, including detached links.
            let record = unsafe { &*raw };
            if record.sequence > self.ceiling {
                break;
            }
            // Cache before invoking the handler: concurrent unlink preserves
            // this successor, and Acquire observes any published initialization.
            raw = record.next.load(Ordering::Acquire);
            callback(&record.action);
        }
    }
}

/// Sole owner of linked ordinary action allocations for one Line.
/// The Line lock serializes membership and sequence assignment. Reclamation
/// of detached nodes belongs to the management transaction, not this collection.
pub(super) struct ActionSet {
    head: *mut ActionRecord,
    tail: *mut ActionRecord,
    count: usize,
    has_shared: bool,
    sequence: usize,
}

// SAFETY: linked allocations retain KBox's unique ownership and cross-CPU
// deallocation contract. Handler = Arc<dyn IrqHandler>, with IrqHandler: Send +
// Sync; all other payload fields are immutable scalars or atomic links. Line
// exclusion serializes head/tail and membership; admitted readers only access
// immutable payloads and atomic next links, not the set itself. Moving the set
// does not move its allocations, and snapshot's contract excludes early Drop.
unsafe impl Send for ActionSet {}

impl ActionSet {
    pub(super) fn new() -> Self {
        Self {
            head: ptr::null_mut(),
            tail: ptr::null_mut(),
            count: 0,
            has_shared: false,
            sequence: 0,
        }
    }

    pub(super) const fn len(&self) -> usize {
        self.count
    }

    /// Scans installed actions under the Line lock for a user-affinity policy.
    ///
    /// This is a control-plane query, not a dispatch-path policy cache.
    pub(super) fn has_user_affinity_restriction(&self) -> bool {
        let mut raw = self.head;
        while !raw.is_null() {
            // SAFETY: the caller holds Line exclusion, which retains linked
            // records and publishes their immutable flags before linkage.
            let record = unsafe { &*raw };
            if record.action.blocks_user_affinity() {
                return true;
            }
            raw = record.next.load(Ordering::Acquire);
        }
        false
    }

    pub(super) fn can_install_regular(&self) -> bool {
        self.count == 0 && self.sequence.checked_add(1).is_some()
    }

    pub(super) fn can_install_shared(&self) -> bool {
        (self.count == 0 || self.has_shared)
            && self.sequence.checked_add(1).is_some()
            && self.count.checked_add(1).is_some()
    }

    pub(super) fn publish_regular(&mut self, prepared: &mut PreparedAction) -> bool {
        if prepared.is_shared() || !self.can_install_regular() {
            return false;
        }
        self.publish(prepared).is_some()
    }

    pub(super) fn publish_shared(
        &mut self,
        prepared: &mut PreparedAction,
    ) -> Option<IrqActionToken> {
        if !prepared.is_shared() || !self.can_install_shared() {
            return None;
        }
        self.publish(prepared).map(IrqActionToken::new)
    }

    fn publish(&mut self, prepared: &mut PreparedAction) -> Option<usize> {
        let sequence = self.sequence.checked_add(1)?;
        let count = self.count.checked_add(1)?;
        let mut record = prepared.pending.take()?;
        record.sequence = sequence;
        if prepared.is_shared() {
            record
                .action
                .set_shared_token(IrqActionToken::new(sequence));
        }
        let raw = KBox::into_raw(record);
        if self.tail.is_null() {
            // Head capture is Line-locked; its unlock/acquire publishes this
            // initialization. Existing readers only follow atomic next links.
            self.head = raw;
        } else {
            // SAFETY: the nonempty set owns its tail; Line exclusion prevents
            // withdrawal while publishing, and only its atomic link is changed.
            unsafe { &(*self.tail).next }.store(raw, Ordering::Release);
        }
        self.tail = raw;
        self.sequence = sequence;
        self.count = count;
        self.has_shared = prepared.is_shared();
        Some(sequence)
    }

    pub(super) fn withdraw(&mut self, token: Option<IrqActionToken>) -> Option<RetiredAction> {
        if self.count == 0 || self.has_shared != token.is_some() {
            return None;
        }
        let mut previous: *mut ActionRecord = ptr::null_mut();
        let mut raw = self.head;
        while !raw.is_null() {
            // SAFETY: Line exclusion keeps all linked nodes owned by this set.
            let record = unsafe { &*raw };
            let next = record.next.load(Ordering::Acquire);
            let is_match = match token {
                Some(token) => record.action.is_shared() && record.action.token() == token,
                None => !record.action.is_shared(),
            };
            if is_match {
                if previous.is_null() {
                    self.head = next;
                } else {
                    // SAFETY: previous is a still-linked node visited under
                    // the same Line exclusion; readers use atomic link loads.
                    unsafe { &(*previous).next }.store(next, Ordering::Release);
                }
                if self.tail == raw {
                    self.tail = previous;
                }
                self.count -= 1;
                if self.count == 0 {
                    self.has_shared = false;
                }
                // Do not reset next: an old reader may already hold this node.
                return Some(RetiredAction { raw });
            }
            previous = raw;
            raw = next;
        }
        None
    }

    /// Captures the head and finite publication ceiling under Line exclusion.
    ///
    /// # Safety
    /// The caller must retain the original Line and protect execution throughout
    /// `'a`, including delayed traversal: ordinary running ownership or the
    /// PerCpu source owner's exclusion of removal until execution completes.
    /// This must keep every reachable linked or detached node alive. Capture
    /// serializes with unlink under the same Line lock. Reclamation must await
    /// all old executions/snapshots; new captures cannot reach an
    /// unlinked node. A private fixture may instead retain all allocations until
    /// its scoped snapshots have been consumed. No snapshot may outlive the set.
    pub(super) unsafe fn snapshot<'a>(&self) -> IrqActionSnapshot<'a> {
        IrqActionSnapshot {
            head: self.head,
            ceiling: self.sequence,
            admission: PhantomData,
        }
    }

    #[cfg(unittest)]
    pub(super) fn exhaust_sequences(&mut self) {
        self.sequence = usize::MAX;
    }
}

impl Drop for ActionSet {
    fn drop(&mut self) {
        let mut raw = self.head;
        while !raw.is_null() {
            // SAFETY: the set uniquely owns only its still-linked nodes. The
            // snapshot contract excludes live readers at destruction; production
            // admission retains the Line owner until traversal and completion end.
            let record = unsafe { KBox::from_raw(raw) };
            raw = record.next.load(Ordering::Acquire);
            drop(record);
        }
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/action_set.rs");
