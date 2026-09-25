// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Timer-side placement on the existing Work pending slot, not a second state owner.

use alloc::sync::Arc;

use super::{
    CompletionObservers, PendingPlacement, PendingSlot, QueuePartition, WorkSlot, WorkState,
};

impl<T> WorkState<T> {
    /// Stores timer-only acceptance after owner validation and successful programming.
    ///
    /// The integrating caller must hold partition -> Work protection, arm the
    /// prepared timer before this infallible publication, and leave the pending
    /// node unlinked. No ordinary queue count or execution allowance is charged.
    ///
    /// # Panics
    /// Panics if protected admission validation was not preserved.
    pub(crate) fn set_delayed_pending(
        &mut self,
        resources: T,
        partition: &Arc<QueuePartition>,
        deadline_ns: u64,
    ) {
        assert!(self.can_accept_on(partition), "admission remains protected");
        self.pending = Some(PendingSlot {
            slot: WorkSlot {
                resources,
                partition: Some(partition.clone()),
                color: None,
                observers: CompletionObservers::new(),
            },
            placement: PendingPlacement::Timer { deadline_ns },
        });
    }

    /// Returns the current authoritative deadline, not a delivered timer's old setting.
    pub(crate) fn delayed_deadline_ns(&self) -> Option<u64> {
        match self.pending.as_ref()?.placement {
            PendingPlacement::Timer { deadline_ns } => Some(deadline_ns),
            _ => None,
        }
    }

    /// Replaces only timer-side placement after successful protected reprogramming.
    /// Keeps acceptance resources, association and existing Work observers intact.
    ///
    /// # Panics
    /// Panics unless the caller validated timer placement under the same guard.
    pub(crate) fn retime_pending(&mut self, deadline_ns: u64) {
        let pending = self.pending.as_mut().expect("validated pending timer");
        assert!(matches!(pending.placement, PendingPlacement::Timer { .. }));
        pending.placement = PendingPlacement::Timer { deadline_ns };
    }

    /// Ends only ordinary queueing, retaining resources and ownership for a timer.
    /// The partition must unlink the node and return any ready allowance under
    /// the same guards. Retire the returned color and notify observers unlocked,
    /// after withdrawal cleanup; neither obligation follows the new timer.
    ///
    /// # Panics
    /// Panics unless an ordinary pending occurrence was validated under protection.
    pub(crate) fn withdraw_pending_to_timer(
        &mut self,
        deadline_ns: u64,
    ) -> (usize, CompletionObservers) {
        let pending = self.pending.as_mut().expect("validated ordinary pending");
        assert!(!matches!(pending.placement, PendingPlacement::Timer { .. }));
        let color = pending
            .slot
            .color
            .take()
            .expect("ordinary pending is counted");
        let observers = pending.slot.observers.take();
        pending.placement = PendingPlacement::Timer { deadline_ns };
        (color, observers)
    }

    /// Transfers timer placement to ordinary waiting without creating an acceptance.
    ///
    /// The partition caller validates the current deadline (or explicit expedition),
    /// reserves the current queue color count, then couples this transition with
    /// intrusive insertion under partition -> Work protection. No resource, observer
    /// or association changes; a running predecessor still blocks release.
    ///
    /// # Panics
    /// Panics unless the caller preserved timer placement under the same guard.
    pub(crate) fn queue_delayed_pending(&mut self, color: usize) {
        let pending = self.pending.as_mut().expect("validated pending timer");
        assert!(matches!(pending.placement, PendingPlacement::Timer { .. }));
        assert!(pending.slot.color.is_none());
        pending.slot.color = Some(color);
        pending.placement = PendingPlacement::Waiting;
    }
}
