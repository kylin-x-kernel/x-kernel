// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-partition ordinary participation counts, independent of Work storage.

use kspin::SpinNoIrq;

use crate::{
    queue::control::COLOR_COUNT,
    work::observers::{CompletionObservers, PreparedObserver},
};

struct ColorCount {
    outstanding: usize,
    observers: CompletionObservers,
}

/// Retains one partition's cleanup obligations for each reusable flush color.
///
/// The retained partition keeps this control alive after semantic Queue resources
/// are released. Lock order is partition, Work, then this control. Color publication,
/// partition barriers and reuse authorization belong to Queue's flush protocol;
/// a zero local count alone never authorizes global color reuse.
pub(super) struct QueueInFlight {
    in_flight: SpinNoIrq<[ColorCount; COLOR_COUNT]>,
}

impl QueueInFlight {
    /// Prepares counters; no storage is allocated by later count operations.
    pub(super) fn new() -> Self {
        Self {
            in_flight: SpinNoIrq::new(core::array::from_fn(|_| ColorCount {
                outstanding: 0,
                observers: CompletionObservers::new(),
            })),
        }
    }

    /// Records a participation before publishing its accepted Work placement.
    ///
    /// The caller holds the semantic partition guard across admission and count
    /// publication. Each successful addition requires exactly one later retirement.
    ///
    /// # Errors
    /// Returns Err without effects when the local count cannot be represented.
    ///
    /// # Panics
    /// Panics if the internal color is outside the prepared array.
    pub(super) fn add(&self, color: usize) -> Result<(), ()> {
        let mut in_flight = self.in_flight.lock();
        let slot = &mut in_flight[color];
        slot.outstanding = slot.outstanding.checked_add(1).ok_or(())?;
        Ok(())
    }

    /// Observes a sealed color after crossing this partition's admission barrier.
    ///
    /// The caller must exclude any subsequent addition to this color until reuse.
    /// An already-empty color returns the unchanged token for unlocked completion;
    /// this method never destroys observer storage or invokes wakeups under locks.
    ///
    /// # Errors
    /// Returns the token when the local scope is already complete.
    ///
    /// # Panics
    /// Panics if the internal color is outside the prepared array.
    pub(super) fn observe(
        &self,
        color: usize,
        observer: PreparedObserver,
    ) -> Result<(), PreparedObserver> {
        let mut in_flight = self.in_flight.lock();
        let slot = &mut in_flight[color];
        if slot.outstanding == 0 {
            return Err(observer);
        }
        slot.observers.attach(observer);
        Ok(())
    }

    /// Retires one original contribution after all covered resources are released.
    ///
    /// This does not inspect Work or a replacement's association. The caller must
    /// complete the returned chain after releasing all management locks. Detaching
    /// notifications prevents old wake delivery from touching a recycled color.
    ///
    /// # Panics
    /// Panics on an invalid color or retirement without an outstanding contribution.
    pub(super) fn retire_after_cleanup(&self, color: usize) -> CompletionObservers {
        let mut in_flight = self.in_flight.lock();
        let slot = &mut in_flight[color];
        slot.outstanding = slot
            .outstanding
            .checked_sub(1)
            .expect("retirement requires an outstanding contribution");
        if slot.outstanding == 0 {
            slot.observers.take()
        } else {
            CompletionObservers::new()
        }
    }
}

#[cfg(unittest)]
mod tests;
