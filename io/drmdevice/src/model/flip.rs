// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! The completion point of one commit.
//!
//! A commit that userspace can wait for allocates two things before it touches
//! the scanout:
//!
//! * the page-flip event slot, so a full event queue fails the *commit* instead
//!   of dropping a completion of a flip that already reached the screen;
//! * the out-fence, so `OUT_FENCE_PTR` returns a descriptor whose readiness is
//!   the flip actually reaching the display.
//!
//! This is Linux's `drm_crtc_state::event` together with its `base.fence`, and
//! the two complete on exactly one point:
//!
//! * a card with a blank timeline queues the flip and completes both on the
//!   blank that scans it out;
//! * a card without one completes the flip with the commit, as
//!   `drm_crtc_send_vblank_event` does for a device without vblank.
//!
//! A commit that fails completes its out-fence with the error instead, and a
//! flip that is replaced before it completed completes it with
//! `DmaFence::set_error` so the failure stays visible. Every allocated fence
//! therefore ends up in a terminal state; [`OutFence`]'s `Drop` is what makes
//! that true even for the paths that return early.

use alloc::sync::Arc;

use super::error::DrmError;
use crate::{prelude::*, sync::dma_fence::FenceSignal};

/// What a commit was asked to complete.
///
/// [`OutFence::new`] is what every commit path calls: it allocates the out-fence
/// the commit's descriptor will name and collects any event slot the caller
/// reserved. `Drop` completes a fence that was never handed to a completion
/// point, so no early return can leave a descriptor pending forever.
pub(crate) struct OutFence<'a> {
    /// The out-fence, until it is handed to its completion point.
    fence: Option<Arc<FenceSignal>>,
    /// Page-flip event slot reserved by the caller.
    event: Option<EventReservation<'a>>,
    /// Client tag carried by the event.
    user_data: u64,
    /// Whether the commit reached its completion point.
    is_settled: bool,
}

impl<'a> OutFence<'a> {
    /// Allocates the completion point of one commit.
    ///
    /// The fence is created before the commit touches the scanout, which is
    /// what lets the commit hand back a descriptor whose readiness is the flip
    /// reaching the display rather than the ioctl returning.
    pub(crate) fn new(
        event: Option<EventReservation<'a>>,
        user_data: u64,
        driver_name: &'static str,
    ) -> Self {
        let (fence, signal) = crate::sync::dma_fence::new_anonymous_fence_named(driver_name);
        let _ = fence;
        Self {
            fence: Some(signal),
            event,
            user_data,
            is_settled: false,
        }
    }

    /// Consumes the parts for a commit that will complete them.
    pub(crate) fn into_flip(mut self) -> Flip<'a> {
        self.is_settled = true;
        Flip {
            event: self.event.take(),
            fence_signal: self.fence.take(),
            user_data: self.user_data,
        }
    }
}

impl Drop for OutFence<'_> {
    fn drop(&mut self) {
        if self.is_settled {
            return;
        }
        // A commit that never reached its completion point: fail its fence so
        // the descriptor it may have handed out becomes readable.
        if let Some(fence) = self.fence.take() {
            fence.signal_error(DrmError::Io.errno());
        }
    }
}

/// The completion point of a successful commit.
///
/// The card owns this value until the flip completes. `Drop` fails a fence that
/// the card never took over, so a panic or an early return between the commit
/// and the queue cannot leave the descriptor pending.
pub(crate) struct Flip<'a> {
    /// The page-flip event slot, when the caller asked for an event.
    pub(crate) event: Option<EventReservation<'a>>,
    /// The out-fence completion.
    pub(crate) fence_signal: Option<Arc<FenceSignal>>,
    /// The client tag `MODE_PAGE_FLIP` / `MODE_ATOMIC` passed.
    pub(crate) user_data: u64,
}

impl Flip<'_> {
    /// Fails the out-fence of a flip the card could not queue.
    pub(crate) fn fail(&mut self, error: DrmError) {
        if let Some(signal) = self.fence_signal.take() {
            signal.signal_error(error.errno());
        }
    }
}

impl Drop for Flip<'_> {
    fn drop(&mut self) {
        // A flip dropped without being completed (a failed commit, a caller
        // that gave up) must not leave its out-fence pending.
        if let Some(signal) = self.fence_signal.take() {
            signal.signal_error(DrmError::Io.errno());
        }
    }
}

/// One flip waiting for the blank that completes it.
pub(crate) struct PendingFlip<'a> {
    /// The slot reserved before the commit; `None` when the client asked for
    /// no event.
    pub(crate) event: Option<EventReservation<'a>>,
    /// The out-fence of the commit; `None` when the client asked for none.
    pub(crate) fence: Option<Arc<crate::sync::dma_fence::FenceSignal>>,
    /// Client tag passed to `MODE_PAGE_FLIP` / `MODE_ATOMIC`.
    pub(crate) user_data: u64,
}

/// Moves an event reservation out of its borrow.
///
/// The reservation borrows the card's event queue. Every open file, and so
/// every ioctl that can create a reservation, keeps the card alive, and
/// `DrmDevice::drop` drains the flip queue before its event queue is
/// destroyed; the returned value only has to live inside that card.
///
/// # Safety
///
/// The caller must ensure the reservation is dropped (or the card's
/// `flip_queue` is drained) before the card's event queue is dropped.
/// `DrmDevice::drop` provides that ordering for every reservation the card
/// owns.
pub(crate) unsafe fn take_reservation(
    event: Option<EventReservation<'_>>,
) -> Option<EventReservation<'static>> {
    let event = event?;
    // SAFETY: the reservation only borrows the event queue, and the caller
    // guarantees that queue outlives the returned value.
    Some(unsafe { core::mem::transmute::<EventReservation<'_>, EventReservation<'static>>(event) })
}
