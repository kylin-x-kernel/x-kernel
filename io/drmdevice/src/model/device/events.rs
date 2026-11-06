// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Event-queue methods of the card.
//!
//! The queue itself lives in `model/event.rs`; these are the card-side access
//! points the ioctl handlers and the minor use.

use crate::prelude::*;

impl DrmDevice {
    /// Reserves room for one flip-complete event.
    ///
    /// The caller reserves before presenting and commits the reservation after
    /// a successful commit, so a full queue fails the ioctl instead of losing
    /// the event for a flip that already reached the screen.
    pub(crate) fn reserve_flip_event(&self) -> Option<EventReservation<'_>> {
        self.events.reserve_flip()
    }

    /// Current vblank sequence number.
    pub(crate) fn vblank_sequence(&self) -> u32 {
        self.events.sequence()
    }

    /// Advances the vblank sequence after a wait.
    pub(crate) fn advance_vblank(&self, count: u32) {
        self.events.advance(count);
    }

    pub(super) fn read_events(&self, buf: &mut [u8]) -> DrmResult<usize> {
        self.backend()?;
        self.events.read(buf)
    }

    pub(super) fn poll_events(&self) -> IoEvents {
        if self.backend().is_err() {
            return IoEvents::ERR | IoEvents::HUP;
        }
        self.events.poll()
    }
}
