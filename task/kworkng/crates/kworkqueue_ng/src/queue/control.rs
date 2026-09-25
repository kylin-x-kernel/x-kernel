// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue's closed/current-color word; color reuse belongs to the flush protocol.

use core::sync::atomic::Ordering;

use super::Queue;

// Four-color ring: one current batch and up to three sealed batches.
// When all colors are occupied, flush waits before its cutoff; admission continues.
pub(super) const COLOR_COUNT: usize = 4;

const _: () = assert!(COLOR_COUNT >= 2 && COLOR_COUNT.is_power_of_two());
const COLOR_MASK: usize = COLOR_COUNT - 1;
pub(super) const CLOSED: usize = COLOR_COUNT;

impl Queue {
    pub(super) fn current_color(&self) -> usize {
        self.control.load(Ordering::SeqCst) & COLOR_MASK
    }

    /// Publishes one cutoff under the flush serializer, preserving concurrent closure.
    /// The returned color must be sealed before releasing that serializer.
    pub(super) fn publish_color(&self, color: usize) -> usize {
        assert!(color < COLOR_COUNT);
        self.control
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |control| {
                Some((control & !COLOR_MASK) | color)
            })
            .expect("unconditional color update")
            & COLOR_MASK
    }

    /// Permanently closes admission; does not wait, cancel, or implement shutdown.
    ///
    /// A submission whose final control sample preceded this publication may
    /// still commit. A future drain must cross every partition admission barrier.
    /// The RMW preserves the color bits for later serialized flush publication.
    pub(super) fn close_admission(&self) {
        self.control.fetch_or(CLOSED, Ordering::SeqCst);
    }

    /// Samples the common cutoff while the accepting partition guard is held.
    ///
    /// Sequential consistency gives control publications and admission samples
    /// one total order across partitions. Work data itself uses partition/Work
    /// locks, not this atomic as a data-publication substitute.
    pub(super) fn admission_color(&self) -> Option<usize> {
        let control = self.control.load(Ordering::SeqCst);
        (control & CLOSED == 0).then_some(control & COLOR_MASK)
    }
}
