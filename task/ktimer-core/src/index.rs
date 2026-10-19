// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Non-owning timer index and node storage, including its owner route.

use core::{
    cell::UnsafeCell,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use intrusive_collections::{KeyAdapter, RBTree, RBTreeAtomicLink, UnsafeRef, intrusive_adapter};
use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrq;

use super::{
    control::TimerControl,
    cpu::CpuTimers,
    deadline::{Deadline, Expiration},
};

/// Borrowed callback data supplied by the timer's external owner.
/// Implementations must suit the dispatch context: no sleeping, unwinding or
/// self-waiting in hard IRQ. Callback-local resources retire before return;
/// the handler itself remains externally owned and may serve repeated events.
pub trait TimerHandler: Sync {
    /// Handles one expiration in hard IRQ context outside timer locks.
    fn on_expiration(&self, timer: TimerControl<'_>, expiration: Expiration);
}

impl<F: for<'call> Fn(TimerControl<'call>, Expiration) + Sync> TimerHandler for F {
    fn on_expiration(&self, timer: TimerControl<'_>, expiration: Expiration) {
        self(timer, expiration);
    }
}

/// The indexed portion of a prepared timer, not a public registration handle.
/// The owner route selects a CpuTimers lock; it never authorizes deadline access.
/// Callback binding is immutable; node and handler storage stay externally owned.
pub(super) struct TimerNode {
    link: RBTreeAtomicLink,
    deadline: UnsafeCell<Deadline>,
    owner_cpu: AtomicUsize,
    handler_data: NonNull<()>,
    call_handler: unsafe fn(NonNull<()>, TimerControl<'_>, Expiration),
}

// SAFETY: safe shared methods only read atomic link/owner fields. All deadline
// reads/writes and index operations are unsafe and require serialization.
// Routing operations validate the owner under its lock before accessing them.
// Callback data is only shared as &H where H: Sync; with_handler pairs the
// pointer with its typed trampoline and invocation requires valid storage.
// This does not grant storage lifetime or permit moving a published node.
unsafe impl Sync for TimerNode {}

// SAFETY: moving an unpublished node moves no handler data. Only a shared
// borrow of H: Sync crosses CPUs; active node movement is forbidden by the
// insertion contract. Plan mutations remain serialized by the owner lock.
unsafe impl Send for TimerNode {}

impl TimerNode {
    /// Binds an unpublished node with an explicit initial lock route.
    ///
    /// # Safety
    /// Uphold with_handler's stable-handler contract. initial_cpu must index
    /// the collection used for all subsequent operations on this node.
    pub(super) unsafe fn with_handler_on<H: TimerHandler>(
        deadline: Deadline,
        handler: &H,
        initial_cpu: LogicalCpuId,
    ) -> Self {
        // SAFETY: the caller retains the bound handler at a stable address.
        // This node is still unpublished, so no route locks are needed yet.
        let mut node = unsafe { Self::with_handler(deadline, handler) };
        node.owner_cpu = AtomicUsize::new(initial_cpu.as_usize());
        node
    }

    pub(super) fn owner_cpu(&self) -> LogicalCpuId {
        LogicalCpuId::new(self.owner_cpu.load(Ordering::Acquire))
    }

    /// Publishes a new route after a non-running node is registered on the new CPU.
    ///
    /// # Safety
    /// Hold both old/new CpuTimers locks in CPU order, revalidate the old route
    /// and absence of running delivery, remove any old membership, and finish
    /// new membership before publication. The new
    /// CPU must index the same stable lock collection used by all operations.
    pub(super) unsafe fn set_owner_cpu(&self, cpu: LogicalCpuId) {
        // Acquire route readers still lock and revalidate; Release publishes
        // the completed migration, not permission for lockless deadline access.
        self.owner_cpu.store(cpu.as_usize(), Ordering::Release);
    }

    /// Membership bit only; the owner must prove which base protects it.
    pub(super) fn is_linked(&self) -> bool {
        self.link.is_linked()
    }

    /// Binds external callback data without allocating or taking ownership.
    ///
    /// # Safety
    /// Keep handler initialized at this address and valid for shared H access
    /// until the node is retired and all callback accesses end. Do not move,
    /// exclusively borrow or start destroying it while a callback can access it.
    /// The handler must obey the dispatch context and must not free its node.
    pub(super) unsafe fn with_handler<H: TimerHandler>(deadline: Deadline, handler: &H) -> Self {
        Self {
            link: RBTreeAtomicLink::new(),
            deadline: UnsafeCell::new(deadline),
            owner_cpu: AtomicUsize::new(0),
            handler_data: NonNull::from(handler).cast(),
            call_handler: Self::call_typed_handler::<H>,
        }
    }

    /// # Safety
    /// data must originate from a live shared H, paired by with_handler;
    /// timer must borrow the current delivery, with its storage still valid.
    unsafe fn call_typed_handler<H: TimerHandler>(
        data: NonNull<()>,
        timer: TimerControl<'_>,
        expiration: Expiration,
    ) {
        // SAFETY: with_handler pairs this function with NonNull::from(&H);
        // dispatch keeps that shared borrow valid under its lifetime contract.
        let handler = unsafe { data.cast::<H>().as_ref() };
        handler.on_expiration(timer, expiration);
    }

    /// Shares the callback binding's provenance for external handler access.
    ///
    /// # Safety
    /// H must be exactly the type bound by with_handler, and that binding's
    /// storage/lifetime contract must hold for the returned borrow. Do not use
    /// this on a placeholder node or expose mutable access to the handler.
    pub(super) unsafe fn handler<H: TimerHandler>(&self) -> &H {
        // SAFETY: the caller proves the typed binding and its live shared
        // storage. Reborrowing this pointer matches callback access even when
        // H contains an UnsafeCell; an unrelated storage borrow must not write it.
        unsafe { self.handler_data.cast::<H>().as_ref() }
    }

    /// Calls the prepared handler without releasing its external storage.
    ///
    /// # Safety
    /// This node must be running in timers, with no concurrent callback for it.
    /// Keep node and handler storage valid as required by with_handler; call
    /// outside timer locks and uphold the handler's execution-context contract.
    pub(super) unsafe fn invoke(&self, timers: &SpinNoIrq<CpuTimers>, expiration: Expiration) {
        // SAFETY: dispatcher pairs this node with its running owner and keeps
        // storage live until the handler and all callback-local borrows end.
        let control = unsafe { TimerControl::new(timers, self) };
        // SAFETY: immutable binding preserves the exact H/function pairing;
        // the dispatch caller guarantees storage, serialization and context.
        unsafe { (self.call_handler)(self.handler_data, control, expiration) };
    }

    /// Unpublished placeholder for owned storage construction and data tests.
    /// Owned storage must replace this binding before its first registration.
    pub(super) const fn new(deadline: Deadline) -> Self {
        Self {
            link: RBTreeAtomicLink::new(),
            deadline: UnsafeCell::new(deadline),
            // A dormant route only; first arm may move to its selected CPU.
            owner_cpu: AtomicUsize::new(0),
            handler_data: NonNull::dangling(),
            call_handler: |_, _, _| {},
        }
    }

    /// Copies the current deadline without borrowing its mutable storage.
    ///
    /// # Safety
    /// The caller must exclude deadline writes for this access (eventually by the
    /// validated owner base lock). A linked node's deadline is never changed.
    pub(super) unsafe fn deadline(&self) -> Deadline {
        // SAFETY: initialized by new; the caller excludes concurrent writes.
        unsafe { *self.deadline.get() }
    }

    /// Replaces a removed node's deadline before reinsertion.
    ///
    /// # Safety
    /// The node must be unlinked and the caller must exclude all deadline reads,
    /// writes and index operations on this node until the replacement completes.
    pub(super) unsafe fn replace_deadline(&self, deadline: Deadline) {
        debug_assert!(!self.link.is_linked());
        // SAFETY: only the initialized UnsafeCell payload is written; exclusive
        // deadline access and removal from all indices are caller preconditions.
        unsafe { *self.deadline.get() = deadline };
    }
}

intrusive_adapter!(NodeAdapter = UnsafeRef<TimerNode>: TimerNode { link => RBTreeAtomicLink });

impl<'a> KeyAdapter<'a> for NodeAdapter {
    type Key = u64;

    fn get_key(&self, node: &'a TimerNode) -> u64 {
        // SAFETY: the adapter is private to this index. Insertion requires
        // stable deadlines until removal, and index operations are serialized.
        unsafe { node.deadline().deadline_ns() }
    }
}

/// One ordered collection and its non-owning minimum-node cache.
///
/// UnsafeRef does not retain or free storage. The caller keeps every inserted
/// node alive at a fixed address, including across index drop, until all index
/// accesses and removed handles end. Owned adapters must drain their pending
/// holds explicitly: dropping this index only unlinks nodes, not Arc counts.
pub(super) struct TimerIndex {
    tree: RBTree<NodeAdapter>,
    earliest: Option<NonNull<TimerNode>>,
}

impl TimerIndex {
    pub(super) const fn new() -> Self {
        Self {
            tree: RBTree::new(NodeAdapter::NEW),
            earliest: None,
        }
    }

    /// Reads the cached minimum in O(1), including a valid u64::MAX deadline.
    pub(super) fn earliest_deadline_ns(&self) -> Option<u64> {
        self.earliest.map(|node| {
            // SAFETY: the cache points to a linked node kept alive by the
            // insertion contract. Linked deadlines cannot change during access.
            let node = unsafe { node.as_ref() };
            // SAFETY: Index access is serialized and linked deadlines stay
            // immutable until the node is removed from this tree.
            unsafe { node.deadline().deadline_ns() }
        })
    }

    /// Inserts an already prepared node without allocating a tree entry.
    ///
    /// # Safety
    /// The node must be unlinked. Keep its address and storage valid until all
    /// index/removed-handle accesses end. Exclude concurrent operations on it,
    /// and do not change its deadline while linked. UnsafeRef itself owns nothing.
    pub(super) unsafe fn insert(&mut self, node: UnsafeRef<TimerNode>) {
        debug_assert!(!node.link.is_linked());
        let ptr = NonNull::from(node.as_ref());
        // SAFETY: insertion requires exclusive deadline access and a live node.
        let deadline_ns = unsafe { node.deadline().deadline_ns() };
        // Pinned intrusive-collections 0.10.2 inserts equal keys to the right,
        // so the existing leftmost node remains the cache entry on equality.
        let is_earliest = self
            .earliest_deadline_ns()
            .is_none_or(|first| deadline_ns < first);
        self.tree.insert(node);
        if is_earliest {
            self.earliest = Some(ptr);
        }
    }

    /// Removes exactly this node, even when other nodes have equal deadlines.
    ///
    /// # Safety
    /// The caller must prove membership in THIS index and exclude concurrent
    /// node operations. link.is_linked alone does not prove which tree owns it.
    pub(super) unsafe fn remove(&mut self, node: &TimerNode) -> UnsafeRef<TimerNode> {
        debug_assert!(node.link.is_linked());
        let was_earliest = self.earliest == Some(NonNull::from(node));
        // SAFETY: the caller proves node belongs to this tree; &mut self
        // serializes tree access and external node operations are excluded.
        let mut cursor = unsafe { self.tree.cursor_mut_from_ptr(node) };
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: The unsafe remove contract proves \
                      membership in this exclusively borrowed timer index"
        )]
        let removed = cursor.remove().expect("timer index member missing");
        if was_earliest {
            // remove leaves the cursor at the successor, now the minimum.
            self.earliest = cursor.get().map(NonNull::from);
        }
        removed
    }

    /// Transfers one due node out of the index; never invokes its callback.
    pub(super) fn pop_due(&mut self, observed_at_ns: u64) -> Option<UnsafeRef<TimerNode>> {
        if self.earliest_deadline_ns()? > observed_at_ns {
            return None;
        }
        let node = self.earliest?;
        // SAFETY: the cache identifies a live member of this exact tree.
        // No mutation occurs between the minimum check and this removal.
        let node = unsafe { node.as_ref() };
        // SAFETY: The cached node still belongs to this exact index; &mut self
        // serializes removal and no deadline/membership change intervened.
        Some(unsafe { self.remove(node) })
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/index_unittest.rs");
