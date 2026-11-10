// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Vblank / page-flip event delivery.
//!
//! The queue is bounded, `read` drains whole records only, and `poll` is the
//! single readiness source for both the event queue and device removal.

use alloc::collections::VecDeque;

use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet};
use ksync::Mutex;

use super::error::{DrmError, DrmResult};
use crate::abi::*;

/// Bounded page-flip/vblank event queue of one card.
pub(crate) struct EventQueue {
    events: Mutex<EventState>,
    poll: PollSet,
}

/// Queued records plus capacity already promised to in-flight flips.
struct EventState {
    events: VecDeque<DrmEventVblank>,
    reserved: usize,
}

/// Capacity promised to one flip that has not committed its event yet.
///
/// A flip reserves its slot before touching the scanout, so a full queue makes
/// the ioctl fail with `ENOMEM` instead of reporting success and dropping the
/// completion event. Dropping an uncommitted reservation returns the slot.
pub(crate) struct EventReservation<'a> {
    queue: &'a EventQueue,
    active: bool,
}

impl EventQueue {
    /// Creates an empty queue.
    pub(crate) fn new() -> Self {
        Self {
            events: Mutex::new(EventState {
                events: VecDeque::with_capacity(MAX_EVENTS),
                reserved: 0,
            }),
            poll: PollSet::new(),
        }
    }

    /// Reserves room for one flip-complete event, or reports a full queue.
    ///
    /// The slot remains reserved until [`EventReservation::commit`] publishes
    /// the event or the reservation is dropped after a failed commit.
    pub(crate) fn reserve_flip(&self) -> Option<EventReservation<'_>> {
        let mut state = self.events.lock();
        if state.events.len() + state.reserved >= MAX_EVENTS {
            return None;
        }
        state.reserved += 1;
        Some(EventReservation {
            queue: self,
            active: true,
        })
    }

    /// Wakes waiters, e.g. after the display disappeared.
    pub(crate) fn wake(&self) {
        self.poll.wake();
    }
}

impl EventReservation<'_> {
    /// Publishes the flip-complete event using the slot reserved earlier.
    ///
    /// The caller names the blank that completed the flip. A page flip on a
    /// display without a blank timeline completes with its commit, which
    /// publishes sequence `0` and the current instant, as Linux's
    /// `drm_crtc_send_vblank_event` does for a device without vblank.
    pub(crate) fn commit_with(
        mut self,
        crtc_id: u32,
        user_data: u64,
        sequence: u32,
        timestamp: ktime_types::TimeSpan,
    ) {
        let now = timestamp;
        let event = DrmEventVblank {
            base: DrmEvent {
                event_type: DRM_EVENT_FLIP_COMPLETE,
                length: core::mem::size_of::<DrmEventVblank>() as u32,
            },
            user_data,
            tv_sec: now.as_secs() as u32,
            tv_usec: now.subsec_micros(),
            sequence,
            crtc_id,
        };
        {
            let mut state = self.queue.events.lock();
            state.reserved -= 1;
            state.events.push_back(event);
        }
        self.active = false;
        self.queue.poll.wake();
    }
}

impl Drop for EventReservation<'_> {
    fn drop(&mut self) {
        if self.active {
            self.queue.events.lock().reserved -= 1;
        }
    }
}

impl EventQueue {
    /// Drains complete event records into `buf`.
    pub(crate) fn read(&self, buf: &mut [u8]) -> DrmResult<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let record = core::mem::size_of::<DrmEventVblank>();
        if buf.len() < record {
            return Err(DrmError::InvalidInput);
        }
        let mut state = self.events.lock();
        let mut written = 0;
        while written + record <= buf.len() {
            let Some(event) = state.events.pop_front() else {
                break;
            };
            buf[written..written + record].copy_from_slice(bytemuck::bytes_of(&event));
            written += record;
        }
        if written == 0 {
            Err(DrmError::WouldBlock)
        } else {
            Ok(written)
        }
    }

    /// Readiness: readable while events are queued.
    pub(crate) fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, !self.events.lock().events.is_empty());
        events
    }

    /// Registers the waiter for read readiness.
    pub(crate) fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll)?;
        }
        Ok(())
    }
}
