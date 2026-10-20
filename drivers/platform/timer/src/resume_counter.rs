// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Counter accounting for guest counter discontinuities.
//!
//! Samples must be serialized within one synchronized counter domain. Forward
//! intervals must be shorter than half the 64-bit counter range. Backward
//! intervals establish a new raw baseline without advancing logical time.
//! Lost sleep time requires an independent trusted clock to reconstruct.

pub(super) struct ResumeCounter {
    last_raw_ticks: u64,
    logical_ticks: u64,
    pending_repair: Option<RepairReport>,
}

pub(super) struct CounterSample {
    pub(super) raw_ticks: u64,
    pub(super) ticks: u64,
    pub(super) is_repaired: bool,
}

/// First detector's diagnostic snapshot, coalesced until IRQ dispatch.
pub(super) struct RepairReport {
    pub(super) cpu: usize,
    pub(super) raw_ticks: u64,
    pub(super) ticks: u64,
}

impl ResumeCounter {
    pub(super) const fn new() -> Self {
        Self {
            last_raw_ticks: 0,
            logical_ticks: 0,
            pending_repair: None,
        }
    }

    pub(super) fn initialize(&mut self, raw_ticks: u64, origin_ticks: u64) {
        self.last_raw_ticks = raw_ticks;
        self.logical_ticks = raw_ticks.saturating_sub(origin_ticks);
    }

    pub(super) fn queue_repair(&mut self, cpu: usize, sample: &CounterSample) {
        if sample.is_repaired && self.pending_repair.is_none() {
            self.pending_repair = Some(RepairReport {
                cpu,
                raw_ticks: sample.raw_ticks,
                ticks: sample.ticks,
            });
        }
    }

    pub(super) fn take_repair(&mut self) -> Option<RepairReport> {
        self.pending_repair.take()
    }

    pub(super) fn sample(&mut self, raw_ticks: u64) -> CounterSample {
        let delta_ticks = raw_ticks.wrapping_sub(self.last_raw_ticks);
        let is_repaired = delta_ticks >= (1u64 << 63);
        self.last_raw_ticks = raw_ticks;
        if !is_repaired {
            self.logical_ticks = self.logical_ticks.saturating_add(delta_ticks);
        }
        CounterSample {
            raw_ticks,
            ticks: self.logical_ticks,
            is_repaired,
        }
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/resume_counter.rs");
