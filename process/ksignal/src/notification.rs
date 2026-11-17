// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel provenance carried alongside, never reconstructed from, signal ABI data.

use alloc::sync::{Arc, Weak};
use core::ptr::NonNull;

use intrusive_collections::{Adapter, DefaultLinkOps, LinkedList, LinkedListAtomicLink};
use kalloc_types::{KArc, KArcPointerOps};
use kspin::{SpinNoIrq, static_lock};

use crate::{SignalDequeueAction, SignalInfo, Signo};

/// Why a timer notification was not installed in a pending queue.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerSignalRejection {
    /// Rearm or retirement invalidated this reservation's generation.
    Invalidated,
    /// The captured manager, configuration or live private slot is gone.
    TargetGone,
    /// The current disposition discards this unblocked notification.
    Ignored,
    /// An ordinary standard send found that signo already pending.
    Coalesced,
}

/// Queue admission result, distinct from a task wake hint or timer completion.
///
/// Rejection returns the original notification without completing its receipt.
/// Consume or release it outside timer, signal and handler ownership locks.
/// Queued does not mean delivered: a later flush can still discard the signal.
#[must_use = "handle returned notification ownership outside locks"]
pub enum TimerSignalSendOutcome {
    /// Ordinary realtime admission could not allocate a record; nothing was published.
    QueueFull,
    /// An ignored periodic notification retains its reservation without a wake.
    Retained,
    /// Pending storage now owns the notification, possibly with all targets blocked.
    Queued {
        /// Optional task wake hint; validate its identity before waking.
        wake_tid: Option<u32>,
    },
    /// Neither a pending nor an ignored queue took ownership.
    Rejected {
        /// Admission decision made under the signal transaction when applicable.
        reason: TimerSignalRejection,
        /// Unconsumed notification, including its unique dequeue authority.
        notification: SignalNotification,
        /// Coalescing preserves the existing wake hint for the already pending signal.
        wake_tid: Option<u32>,
    },
}

impl TimerSignalSendOutcome {
    /// Returns a wake hint, never proof of queue admission or completion.
    pub fn wake_tid(&self) -> Option<u32> {
        match self {
            Self::Retained | Self::QueueFull => None,
            Self::Queued { wake_tid } | Self::Rejected { wake_tid, .. } => *wake_tid,
        }
    }

    pub(crate) fn admission_result(&self) -> kerrno::KResult<()> {
        if matches!(self, Self::QueueFull) {
            Err(kerrno::KError::WouldBlock)
        } else {
            Ok(())
        }
    }

    pub(crate) fn rejected(notification: SignalNotification, reason: TimerSignalRejection) -> Self {
        Self::Rejected {
            reason,
            notification,
            wake_tid: None,
        }
    }

    pub(crate) fn from_queue(rejected: Option<SignalNotification>, wake_tid: Option<u32>) -> Self {
        match rejected {
            Some(notification) => Self::Rejected {
                reason: TimerSignalRejection::Coalesced,
                notification,
                wake_tid,
            },
            None => Self::Queued { wake_tid },
        }
    }
}

/// Stable signal destination captured before publishing a POSIX timer.
///
/// Weak manager ownership avoids cycles. A private destination additionally
/// retains its non-wrapping slot identity, never a reusable numeric TID.
/// Cloning allocates no storage. Release the last weak reference outside locks.
#[derive(Clone)]
pub struct TimerSignalTarget {
    manager: Weak<crate::api::ProcessSignalManager>,
    slot: Option<crate::SlotId>,
}

impl TimerSignalTarget {
    /// Returns a live, unblocked wake hint for a currently pending notification.
    /// Call outside timer/signal locks and validate matches_thread before waking.
    pub fn pending_wake_tid(&self, signo: Signo) -> Option<u32> {
        let manager = self.manager.upgrade()?;
        manager.pending_timer_wake_tid(self.slot, signo)
    }

    /// Captures a shared pending queue, without keeping its manager alive.
    pub fn process(manager: Weak<crate::api::ProcessSignalManager>) -> Self {
        Self {
            manager,
            slot: None,
        }
    }

    pub(crate) fn thread(
        manager: Weak<crate::api::ProcessSignalManager>,
        slot: crate::SlotId,
    ) -> Self {
        Self {
            manager,
            slot: Some(slot),
        }
    }

    /// Sends through the captured identity, returning admission and ownership.
    ///
    /// Expired managers, retired slots and exited private targets are rejected.
    /// Reserved leases bypass ordinary standard coalescing. Ignored periodic
    /// leases are retained without a wake; one-shots are rejected. Call without
    /// timer or signal locks held. Rejection does
    /// not acknowledge a timer. Rejected notifications are returned to the
    /// unlocked bridge, not destroyed here. Verify matches_thread before using
    /// the wake TID. Retention never acknowledges delivery.
    pub fn send(&self, notification: SignalNotification) -> TimerSignalSendOutcome {
        let Some(manager) = self.manager.upgrade() else {
            return TimerSignalSendOutcome::rejected(
                notification,
                TimerSignalRejection::TargetGone,
            );
        };
        if let Some(slot) = self.slot {
            crate::api::ThreadSignalManager::send_to_slot(&manager, slot, notification, true).1
        } else {
            manager.send_signal_inner(notification, true)
        }
    }

    /// Checks a looked-up task's signal identity before the caller wakes it.
    ///
    /// Numeric lookup is only a wake hint, never authority to enqueue. The caller
    /// must keep the checked task alive through its wake operation.
    pub fn matches_thread(&self, thread: &crate::api::ThreadSignalManager) -> bool {
        let Some(manager) = self.manager.upgrade() else {
            return false;
        };
        Arc::ptr_eq(&manager, thread.process())
            && self.slot.is_none_or(|slot| slot == thread.slot_id())
            && manager
                .siglock()
                .inspect(|view| view.target_tid(thread.slot_id()))
                .is_some()
    }
}

/// Originating runtime authority for a kernel timer notification.
///
/// Implementations validate the timer ID and generation in their own state,
/// never by looking up the receiving task or a reusable process ID. Completion
/// runs outside signal locks; it may acquire the originating timer lock.
pub trait TimerSignalOrigin: Send + Sync {
    /// Validates and acknowledges one dequeued notification, or rejects stale state.
    fn complete(&self, timer_id: i32, signal_seq: u64) -> SignalDequeueAction;

    /// Retries deferred demand after checking this exact reservation's identity.
    /// Runs outside signal, record and ready locks; rejection is not completion.
    /// A coalesced requeue request can invoke this while the receipt is occupied;
    /// implementations must not treat this callback as proof of free storage.
    fn record_available(&self, _timer_id: i32, _record: &PreparedTimerRecord) {}

    /// Wakes a requeued notification after validating its reservation identity.
    /// Runs on the existing ready worker, outside all signal/record locks.
    fn record_requeued(&self, _timer_id: i32, _record: &PreparedTimerRecord) {}
}

/// A queued signal and its kernel-only dequeue provenance.
///
/// Converting ABI data through `From<SignalInfo>` always produces an ordinary
/// notification, including when its code is SI_TIMER. Prepared timer receipts
/// additionally own a unique reservation lease. Dropping invokes no completion.
pub struct SignalNotification {
    info: SignalInfo,
    receipt: Option<TimerSignalReceipt>,
    can_lose_details: bool,
}

/// Unique dequeue authority for a kernel-originated POSIX timer notification.
///
/// The weak runtime identity cannot keep its payload alive or form a pending-
/// queue/runtime ownership cycle. Completion rejects an expired origin. The
/// internal u64 generation is independent of siginfo. A prepared receipt holds
/// its lease until completion returns and the receipt drops. Consumption and weak release occur outside
/// signal locks, and the receipt is deliberately not Clone.
pub struct TimerSignalReceipt {
    timer_id: i32,
    signal_seq: u64,
    origin: Weak<dyn TimerSignalOrigin>,
    lease: Option<TimerRecordLease>,
    is_periodic: bool,
}

impl TimerSignalReceipt {
    /// Returns the originating timer's ABI identifier.
    pub const fn timer_id(&self) -> i32 {
        self.timer_id
    }

    /// Returns the originating notification's existing sequence number.
    pub const fn signal_seq(&self) -> u64 {
        self.signal_seq
    }

    pub(crate) fn complete(self) -> SignalDequeueAction {
        self.origin
            .upgrade()
            .map_or(SignalDequeueAction::Drop, |origin| {
                origin.complete(self.timer_id, self.signal_seq)
            })
    }
}

impl SignalNotification {
    /// Creates an ordinary kernel notification that must survive owner exhaustion.
    ///
    /// Use only for audited kernel-generated events, never for supplied userspace
    /// siginfo. Allocation still uses NOWAIT; failure preserves only a pending bit.
    /// This grants no timer receipt or completion authority.
    pub fn kernel_fallback(info: SignalInfo) -> Self {
        Self {
            info,
            receipt: None,
            can_lose_details: true,
        }
    }

    pub(crate) fn can_lose_details(&self) -> bool {
        self.receipt.is_none()
            && (self.can_lose_details
                || !self.signo().is_realtime()
                || self.info.code() == linux_raw_sys::general::SI_USER as i32)
    }

    /// Builds an unreserved kernel timer notification for compatibility tests.
    ///
    /// Crate-local fixtures use this to test provenance independently of storage
    /// reservation. Production timer emission uses PreparedTimerRecord::prepare.
    /// Completion upgrades the supplied weak origin outside signal locks;
    /// coalescing, rejection and queue destruction do not acknowledge it.
    /// Siginfo retains only the low 32 generation bits for ABI compatibility;
    /// completion uses the full generation stored in the private receipt.
    #[cfg(unittest)]
    pub(crate) fn timer(
        signo: Signo,
        timer_id: i32,
        overrun: i32,
        value_bits: usize,
        signal_seq: u64,
        origin: Weak<dyn TimerSignalOrigin>,
    ) -> Self {
        Self {
            info: SignalInfo::new_timer(signo, timer_id, overrun, value_bits, signal_seq as u32),
            receipt: Some(TimerSignalReceipt {
                timer_id,
                signal_seq,
                origin,
                lease: None,
                is_periodic: false,
            }),
            can_lose_details: false,
        }
    }

    /// Returns the signal number without exposing or consuming its receipt.
    pub fn signo(&self) -> Signo {
        self.info.signo()
    }

    /// Borrows the immutable ABI payload without transferring authority.
    pub fn info(&self) -> &SignalInfo {
        &self.info
    }

    /// Borrows provenance for diagnostics without duplicating the lease.
    pub fn receipt(&self) -> Option<&TimerSignalReceipt> {
        self.receipt.as_ref()
    }

    pub(crate) fn into_parts(self) -> (SignalInfo, Option<TimerSignalReceipt>) {
        (self.info, self.receipt)
    }
}

impl From<SignalInfo> for SignalNotification {
    fn from(info: SignalInfo) -> Self {
        Self {
            info,
            receipt: None,
            can_lose_details: false,
        }
    }
}

struct RecordState {
    is_occupied: bool,
    invalidated_through: Option<u64>,
    notification: Option<SignalNotification>,
    ready_timer_id: i32,
    needs_requeue_wake: bool,
}

pub(crate) struct NotificationNode {
    pending: LinkedListAtomicLink,
    ready: LinkedListAtomicLink,
    origin: Option<Weak<dyn TimerSignalOrigin>>,
    state: SpinNoIrq<RecordState>,
}

impl NotificationNode {
    pub(crate) fn allocate_ordinary(
        info: SignalInfo,
        allocate: impl FnOnce(Self) -> Option<KArc<Self>>,
    ) -> Option<KArc<Self>> {
        let mut node = Self::new(None);
        node.state.get_mut().notification = Some(info.into());
        allocate(node)
    }

    pub(crate) fn signo(&self) -> Option<Signo> {
        self.state
            .lock()
            .notification
            .as_ref()
            .map(SignalNotification::signo)
    }

    pub(crate) fn can_retain_ignored(&self) -> bool {
        let state = self.state.lock();
        self.origin.is_some()
            && state
                .notification
                .as_ref()
                .and_then(|notification| notification.receipt.as_ref())
                .is_some_and(|receipt| {
                    receipt.is_periodic
                        && state
                            .invalidated_through
                            .is_none_or(|through| receipt.signal_seq > through)
                })
    }

    pub(crate) fn schedule_requeue_wake(owner: &KArc<Self>) {
        let mut ready = READY_RECORDS.lock();
        let mut state = owner.state.lock();
        let Some(receipt) = state
            .notification
            .as_ref()
            .and_then(|notification| notification.receipt.as_ref())
        else {
            return;
        };
        state.ready_timer_id = receipt.timer_id;
        state.needs_requeue_wake = true;
        if !owner.ready.is_linked() {
            ready.queue.push_back(owner.clone());
        }
    }

    fn new(origin: Option<Weak<dyn TimerSignalOrigin>>) -> Self {
        Self {
            pending: LinkedListAtomicLink::new(),
            ready: LinkedListAtomicLink::new(),
            origin,
            state: SpinNoIrq::new(RecordState {
                is_occupied: false,
                invalidated_through: None,
                notification: None,
                ready_timer_id: 0,
                needs_requeue_wake: false,
            }),
        }
    }

    pub(crate) fn into_notification(owner: &KArc<Self>) -> Option<SignalNotification> {
        let mut notification = owner.state.lock().notification.take()?;
        if owner.origin.is_some()
            && let Some(receipt) = notification.receipt.as_mut()
        {
            receipt.lease = Some(TimerRecordLease(Some(owner.clone()), receipt.timer_id));
        }
        Some(notification)
    }
}

/// Reserved storage for exactly one outstanding timer receipt.
///
/// Creation may sleep; prepare, enqueue and dequeue allocate nothing. The
/// reservation stays occupied through completion until the unique receipt is
/// dropped. Final owners must be released outside timer and signal locks.
/// Cloning shares storage identity only; it cannot duplicate a live lease.
#[derive(Clone)]
pub struct PreparedTimerRecord {
    node: KArc<NotificationNode>,
}

/// Invalidated notification ownership awaiting removal from its captured queue.
///
/// Drop outside timer and signal locks. Removal never acknowledges delivery and
/// cannot remove a newer generation or a different reservation with the same ID.
#[must_use = "release cancellation outside timer and signal locks"]
pub struct TimerSignalCancellation {
    record: PreparedTimerRecord,
    target: TimerSignalTarget,
    through: u64,
}

impl Drop for TimerSignalCancellation {
    fn drop(&mut self) {
        let Some(manager) = self.target.manager.upgrade() else {
            return;
        };
        let detached = manager
            .siglock()
            .transact(|txn| txn.cancel_timer(self.target.slot, &self.record, self.through));
        drop(detached);
    }
}

impl PreparedTimerRecord {
    /// Invalidates generations through `through` without acquiring signal locks.
    ///
    /// Call while serializing timer mutation, then drop the returned cancellation
    /// after unlocking. Admission serialized before invalidation is removed by
    /// cancellation; admission after invalidation rejects the old lease.
    pub fn invalidate(&self, target: &TimerSignalTarget, through: u64) -> TimerSignalCancellation {
        self.invalidate_through(through);
        TimerSignalCancellation {
            record: self.clone(),
            target: target.clone(),
            through,
        }
    }

    /// Rejects old admission immediately; the owner must arrange unlocked queue
    /// cancellation separately through [`Self::invalidate`]. No references drop.
    pub fn invalidate_through(&self, through: u64) {
        let mut state = self.node.state.lock();
        state.invalidated_through = Some(
            state
                .invalidated_through
                .map_or(through, |old| old.max(through)),
        );
    }

    pub(crate) fn pending_signo(&self, through: u64) -> Option<Signo> {
        let state = self.node.state.lock();
        let notification = state.notification.as_ref()?;
        (notification.receipt.as_ref()?.signal_seq <= through).then(|| notification.signo())
    }

    pub(crate) fn node_ref(&self) -> &KArc<NotificationNode> {
        &self.node
    }

    /// Allocates once with KERNEL permissions, before taking the timer lock.
    /// Returns NoMemory if storage cannot be reserved.
    pub fn new(origin: Weak<dyn TimerSignalOrigin>) -> kerrno::KResult<Self> {
        Ok(Self {
            node: KArc::new(NotificationNode::new(Some(origin)))?,
        })
    }

    /// Tests reservation identity without allocating or acquiring a lock.
    pub fn same_storage(&self, other: &Self) -> bool {
        KArc::ptr_eq(&self.node, &other.node)
    }

    /// Claims the sole lease, returning None when busy or already invalidated.
    /// Queued data cannot be overwritten by a later generation.
    /// is_periodic snapshots the timer interval under the timer's own lock;
    /// it authorizes retention on ignore, never an acknowledgement.
    pub fn prepare(
        &self,
        signo: Signo,
        timer_id: i32,
        overrun: i32,
        value_bits: usize,
        signal_seq: u64,
        is_periodic: bool,
    ) -> Option<SignalNotification> {
        let origin = self.node.origin.as_ref()?;
        let mut state = self.node.state.lock();
        if state.is_occupied
            || state
                .invalidated_through
                .is_some_and(|through| signal_seq <= through)
        {
            return None;
        }
        state.is_occupied = true;
        Some(SignalNotification {
            info: SignalInfo::new_timer(signo, timer_id, overrun, value_bits, signal_seq as u32),
            receipt: Some(TimerSignalReceipt {
                timer_id,
                signal_seq,
                origin: origin.clone(),
                lease: Some(TimerRecordLease(Some(self.node.clone()), timer_id)),
                is_periodic,
            }),
            can_lose_details: false,
        })
    }
}

struct TimerRecordLease(Option<KArc<NotificationNode>>, i32);

impl Drop for TimerRecordLease {
    fn drop(&mut self) {
        let Some(node) = self.0.take() else { return };
        let wake = {
            let mut ready = READY_RECORDS.lock();
            {
                let mut state = node.state.lock();
                state.is_occupied = false;
                state.ready_timer_id = self.1;
            }
            if !node.ready.is_linked() {
                ready.queue.push_back(node.clone());
            }
            ready.waker
        };
        if let Some(wake) = wake {
            wake();
        }
    }
}

impl SignalNotification {
    pub(crate) fn can_retain_ignored(&self) -> bool {
        self.has_reserved_storage()
            && self
                .receipt
                .as_ref()
                .is_some_and(|receipt| receipt.is_periodic)
            && !self.is_invalidated()
    }

    /// Only a live reserved lease may bypass ordinary standard-signal coalescing.
    pub(crate) fn has_reserved_storage(&self) -> bool {
        self.receipt
            .as_ref()
            .and_then(|receipt| receipt.lease.as_ref())
            .is_some_and(|lease| lease.0.is_some())
    }

    pub(crate) fn is_invalidated(&self) -> bool {
        self.receipt.as_ref().is_some_and(|receipt| {
            receipt
                .lease
                .as_ref()
                .and_then(|lease| lease.0.as_ref())
                .is_some_and(|node| {
                    node.state
                        .lock()
                        .invalidated_through
                        .is_some_and(|through| receipt.signal_seq <= through)
                })
        })
    }

    /// Transfers the lease's owner to the queue without a self-reference.
    /// Used for reserved records and unreserved receipt compatibility fixtures.
    pub(crate) fn into_node(mut self) -> KArc<NotificationNode> {
        let reserved = self
            .receipt
            .as_mut()
            .and_then(|receipt| receipt.lease.take().and_then(|mut lease| lease.0.take()));
        let node = reserved.unwrap_or_else(|| {
            KArc::new_nowait(NotificationNode::new(None)).unwrap_or_else(|_| {
                alloc::alloc::handle_alloc_error(core::alloc::Layout::new::<NotificationNode>())
            })
        });
        {
            let mut state = node.state.lock();
            state.notification = Some(self);
        }
        node
    }
}

#[derive(Clone)]
pub(crate) struct NotificationAdapter<const READY: bool> {
    links: <LinkedListAtomicLink as DefaultLinkOps>::Ops,
    pointers: KArcPointerOps<NotificationNode>,
}

impl<const READY: bool> NotificationAdapter<READY> {
    const OFFSET: usize = if READY {
        core::mem::offset_of!(NotificationNode, ready)
    } else {
        core::mem::offset_of!(NotificationNode, pending)
    };

    pub(crate) const fn new() -> Self {
        Self {
            links: <LinkedListAtomicLink as DefaultLinkOps>::NEW,
            pointers: KArcPointerOps::new(),
        }
    }
}

/// SAFETY: Both projections use the same field offset in a stable KArc
/// allocation. Each list owns one transferred strong reference. Atomic links
/// permit independent pending and ready ownership across CPUs; membership is
/// serialized by the pending queue owner or READY_RECORDS respectively.
unsafe impl<const READY: bool> Adapter for NotificationAdapter<READY> {
    type LinkOps = <LinkedListAtomicLink as DefaultLinkOps>::Ops;
    type PointerOps = KArcPointerOps<NotificationNode>;

    /// SAFETY: Adapter supplies the selected field of a live NotificationNode;
    /// subtracting its offset recovers the allocation's original value pointer.
    unsafe fn get_value(&self, link: NonNull<LinkedListAtomicLink>) -> *const NotificationNode {
        link.as_ptr().wrapping_byte_sub(Self::OFFSET).cast()
    }

    /// SAFETY: Adapter supplies a live node; adding the selected field offset
    /// yields its aligned, non-null link without creating a mutable reference.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: Adapter supplies a live node and the \
                  selected field lies within that non-null allocation"
    )]
    unsafe fn get_link(&self, value: *const NotificationNode) -> NonNull<LinkedListAtomicLink> {
        NonNull::new(value.wrapping_byte_add(Self::OFFSET).cast_mut().cast())
            .expect("live notification node link")
    }

    fn link_ops(&self) -> &Self::LinkOps {
        &self.links
    }

    fn link_ops_mut(&mut self) -> &mut Self::LinkOps {
        &mut self.links
    }

    fn pointer_ops(&self) -> &Self::PointerOps {
        &self.pointers
    }
}

struct ReadyRecords {
    queue: LinkedList<NotificationAdapter<true>>,
    waker: Option<fn()>,
    is_running: bool,
}

static_lock! {
    static READY_RECORDS: SpinNoIrq<ReadyRecords> = SpinNoIrq::new(ReadyRecords {
        queue: LinkedList::new(NotificationAdapter::new()),
        waker: None,
        is_running: false,
    });
}

/// Installs the alarm wake hook, notifying it of work queued before registration.
/// The hook must only schedule work and must not synchronously process records.
pub fn register_timer_record_waker(waker: fn()) {
    let has_work = {
        let mut ready = READY_RECORDS.lock();
        ready.waker = Some(waker);
        !ready.queue.is_empty()
    };
    if has_work {
        waker();
    }
}

/// Notifies the existing worker after a requeue transaction has unlocked.
pub(crate) fn wake_ready_records() {
    let wake = {
        let ready = READY_RECORDS.lock();
        if ready.queue.is_empty() {
            None
        } else {
            ready.waker
        }
    };
    if let Some(wake) = wake {
        wake();
    }
}

/// Processes one availability notification outside all signal/ready/record locks.
/// Call without outer locks. One runner is serialized globally, with one queued
/// rerun per record. Returns false if empty or another runner is active.
/// Requeue requests share the ready link but retain their occupied receipt.
/// Neither availability nor requeue wake acknowledges delivery.
pub fn process_one_ready_timer_record() -> bool {
    let (node, timer_id, needs_requeue_wake) = {
        let mut ready = READY_RECORDS.lock();
        if ready.is_running {
            return false;
        }
        let Some(node) = ready.queue.pop_front() else {
            return false;
        };
        let (timer_id, needs_requeue_wake) = {
            let mut state = node.state.lock();
            (
                state.ready_timer_id,
                core::mem::take(&mut state.needs_requeue_wake),
            )
        };
        ready.is_running = true;
        (node, timer_id, needs_requeue_wake)
    };
    let record = PreparedTimerRecord { node };
    if let Some(origin) = record.node.origin.as_ref().and_then(Weak::upgrade) {
        origin.record_available(timer_id, &record);
        if needs_requeue_wake {
            origin.record_requeued(timer_id, &record);
        }
    }
    let wake = {
        let mut ready = READY_RECORDS.lock();
        ready.is_running = false;
        if ready.queue.is_empty() {
            None
        } else {
            ready.waker
        }
    };
    if let Some(wake) = wake {
        wake();
    }
    true
}
