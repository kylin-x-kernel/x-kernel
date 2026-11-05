// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pending signal queues for processes and threads.
use core::array;

use intrusive_collections::LinkedList;
use kalloc_types::KArc;
use kerrno::{KError, KResult};

use crate::{
    MAX_SIGNALS, SignalInfo, SignalNotification, SignalSet, TimerSignalReceipt,
    notification::{NotificationAdapter, NotificationNode},
};

/// Notification storage transferred out of a pending queue without freeing it.
pub(crate) enum DetachedNotification {
    Stored(KArc<NotificationNode>),
    BitOnly(crate::Signo),
}

impl DetachedNotification {
    /// Consumes the detached owner after the enclosing signal lock is released.
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: detached queue owners contain exactly one \
                  notification until consumed"
    )]
    pub(crate) fn into_parts(self) -> (SignalInfo, Option<TimerSignalReceipt>) {
        match &self {
            Self::Stored(node) => NotificationNode::into_notification(node)
                .expect("detached notification payload")
                .into_parts(),
            Self::BitOnly(signo) => (SignalInfo::without_sender(*signo), None),
        }
    }
}

impl Drop for DetachedNotification {
    fn drop(&mut self) {
        if let Self::Stored(node) = self {
            drop(NotificationNode::into_notification(node));
        }
    }
}

/// Removed owners whose payloads must be destroyed after the signal transaction.
pub(crate) struct DiscardedNotifications(LinkedList<NotificationAdapter<false>>);

impl Default for DiscardedNotifications {
    fn default() -> Self {
        Self(LinkedList::new(NotificationAdapter::new()))
    }
}

impl Drop for DiscardedNotifications {
    fn drop(&mut self) {
        while let Some(node) = self.0.pop_front() {
            drop(DetachedNotification::Stored(node));
        }
    }
}

/// Queue for managing pending signals awaiting delivery.
///
/// Each signo owns a FIFO shared by ordinary and prepared notifications.
/// Ordinary standard sends coalesce whenever that signo is already pending.
/// Distinct prepared timer records bypass that check, including for standard
/// signals; each timer's unique lease still permits only one queued instance.
/// Ignored periodic records retain that same link in a separate list, without
/// setting pending bits, until replay, cancellation or destruction.
pub struct PendingSignals {
    /// Bitmask of all pending signals
    pub set: SignalSet,

    /// Same-signo arrival order, indexed by signal number minus one.
    queues: [LinkedList<NotificationAdapter<false>>; MAX_SIGNALS],
    ignored: LinkedList<NotificationAdapter<false>>,
}

impl Default for PendingSignals {
    fn default() -> Self {
        Self {
            set: SignalSet::default(),
            queues: array::from_fn(|_| LinkedList::new(NotificationAdapter::new())),
            ignored: LinkedList::new(NotificationAdapter::new()),
        }
    }
}

impl PendingSignals {
    pub(crate) fn retain_ignored(&mut self, notification: SignalNotification) {
        self.ignored.push_back(notification.into_node());
    }

    pub(crate) fn ignore(&mut self, signo: crate::Signo, discarded: &mut DiscardedNotifications) {
        while let Some(node) = self.queues[signo as usize - 1].pop_front() {
            if node.can_retain_ignored() {
                self.ignored.push_back(node);
            } else {
                discarded.0.push_back(node);
            }
        }
        self.set.remove(signo);
    }

    pub(crate) fn replay(
        &mut self,
        mask: SignalSet,
        has_target: bool,
        discarded: &mut DiscardedNotifications,
    ) -> bool {
        let mut has_replayed = false;
        let mut cursor = self.ignored.front_mut();
        while let Some(node) = cursor.get() {
            let Some(signo) = node.signo().filter(|signo| mask.has(*signo)) else {
                cursor.move_next();
                continue;
            };
            let can_replay = has_target && node.can_retain_ignored();
            if let Some(node) = cursor.remove() {
                if can_replay {
                    NotificationNode::schedule_requeue_wake(&node);
                    self.queues[signo as usize - 1].push_back(node);
                    self.set.add(signo);
                    has_replayed = true;
                } else {
                    discarded.0.push_back(node);
                }
            }
        }
        has_replayed
    }

    pub(crate) fn hide_ignored(
        &mut self,
        mut mask: SignalSet,
        discarded: &mut DiscardedNotifications,
    ) {
        while let Some(signo) = mask.dequeue(&self.set) {
            self.ignore(signo, discarded);
        }
    }

    /// Cold-path cancellation searches the recorded FIFO then the ignored list, using
    /// exact owner identity rather than unsafe cross-queue cursor reconstruction.
    pub(crate) fn cancel_timer(
        &mut self,
        record: &crate::PreparedTimerRecord,
        through: u64,
    ) -> Option<DetachedNotification> {
        let signo = record.pending_signo(through)?;
        let queue = &mut self.queues[signo as usize - 1];
        let mut cursor = queue.front_mut();
        while let Some(node) = cursor.get() {
            if core::ptr::eq(node, &**record.node_ref()) {
                break;
            }
            cursor.move_next();
        }
        let removed = cursor.remove();
        if removed.is_some() && queue.is_empty() {
            self.set.remove(signo);
        }
        if let Some(removed) = removed {
            return Some(DetachedNotification::Stored(removed));
        }
        let mut cursor = self.ignored.front_mut();
        while let Some(node) = cursor.get() {
            if core::ptr::eq(node, &**record.node_ref()) {
                return cursor.remove().map(DetachedNotification::Stored);
            }
            cursor.move_next();
        }
        None
    }

    /// Adds a signal to the pending queue.
    ///
    /// Ordinary standard signals (1-31) coalesce while any instance is pending,
    /// including a prepared timer. Real-time signals (32-64) attempt an append
    /// on every send, including when a previous send left only a pending bit.
    ///
    /// # Arguments
    /// * `sig` - Signal information to add
    ///
    /// Ordinary notifications allocate a KArc node with NOWAIT only after
    /// admission. Ordinary standard allocation failure preserves the pending bit
    /// without detailed siginfo; ordinary SIGKILL needs no storage. Realtime
    /// SI_USER failure also preserves the bit. Other ordinary realtime failures
    /// return WouldBlock without changing the queue or pending set.
    /// The reference-count overhead is accepted
    /// so ordinary and prepared timer notifications share the same FIFO.
    /// Prepared timer notifications reuse their reserved node without allocation.
    ///
    /// # Returns
    /// `true` if the signal was successfully added, `false` if it was
    /// already pending (for standard signals only)
    ///
    /// # Errors
    /// Returns WouldBlock when an ordinary non-SI_USER realtime owner cannot
    /// be allocated. The queue and pending bitmap are unchanged on failure.
    pub fn put_signal(&mut self, sig: SignalInfo) -> KResult<bool> {
        Ok(self.put_notification(sig.into())?.is_none())
    }

    /// Returns unqueued ownership for release outside the enclosing signal lock.
    pub(crate) fn put_notification(
        &mut self,
        sig: SignalNotification,
    ) -> KResult<Option<SignalNotification>> {
        self.put_notification_with_allocator(sig, |node| KArc::new_nowait(node).ok())
    }

    pub(crate) fn put_notification_with_allocator(
        &mut self,
        sig: SignalNotification,
        allocate: impl FnOnce(NotificationNode) -> Option<KArc<NotificationNode>>,
    ) -> KResult<Option<SignalNotification>> {
        let signo = sig.signo();
        if !signo.is_realtime() && self.set.has(signo) && !sig.has_reserved_storage() {
            return Ok(Some(sig));
        }
        if sig.receipt().is_none() {
            let can_lose_details = sig.can_lose_details();
            if signo != crate::Signo::SIGKILL {
                match NotificationNode::allocate_ordinary(sig.into_parts().0, allocate) {
                    Some(node) => self.queues[signo as usize - 1].push_back(node),
                    None if can_lose_details => {}
                    None => return Err(KError::WouldBlock),
                }
            }
        } else {
            self.queues[signo as usize - 1].push_back(sig.into_node());
        }
        self.set.add(signo);
        Ok(None)
    }

    /// Returns whether any queued signal is deliverable under `blocked`.
    ///
    /// This is the authoritative deliverability predicate: it is evaluated
    /// only by the owner of the queue, inside the same critical section that
    /// mutates it, and its result is what the lock-free fast-path projection
    /// is derived from.
    pub(crate) fn has_deliverable(&self, blocked: SignalSet) -> bool {
        !(self.set & !blocked).is_empty()
    }

    /// Dequeues ordinary data inserted through [`Self::put_signal`].
    ///
    /// Manager-owned queues use the internal notification dequeue so timer
    /// authority is transferred to their unlocked completion path instead.
    pub fn dequeue_signal(&mut self, mask: &SignalSet) -> Option<SignalInfo> {
        self.dequeue_notification(mask)
            .map(|notification| notification.into_parts().0)
    }

    pub(crate) fn dequeue_notification(
        &mut self,
        mask: &SignalSet,
    ) -> Option<DetachedNotification> {
        self.set.dequeue(mask).and_then(|signo| {
            let queue = &mut self.queues[signo as usize - 1];
            let result = queue.pop_front();
            if !queue.is_empty() {
                self.set.add(signo);
            }
            result
                .map(DetachedNotification::Stored)
                .or(Some(DetachedNotification::BitOnly(signo)))
        })
    }
}

/// Entire queues are detached by their manager before destruction, so payload,
/// lease and final strong/weak releases occur outside the outer signal locks.
impl Drop for PendingSignals {
    fn drop(&mut self) {
        while let Some(node) = self.ignored.pop_front() {
            drop(DetachedNotification::Stored(node));
        }
        for queue in &mut self.queues {
            while let Some(node) = queue.pop_front() {
                drop(DetachedNotification::Stored(node));
            }
        }
    }
}
