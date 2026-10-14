// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue-independent identity and best-effort backlog observation.

use alloc::boxed::Box;

use kcpu_id_map::LogicalCpuId;
use kspin::SpinNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};

/// Partition owns the record; a monitor may temporarily retain it, never Queue/Work.
/// It contains only diagnostic data, so its final Drop needs no callback or drain.
pub(crate) struct StallRecord {
    pub(super) name: Box<str>,
    pub(super) cpu: LogicalCpuId,
    state: SpinNoIrq<StallState>,
}

struct StallState {
    interval_start: Option<MonotonicInstant>,
    last_warning: Option<MonotonicInstant>,
}

impl StallRecord {
    pub(super) fn new(name: Box<str>, cpu: LogicalCpuId) -> Self {
        Self {
            name,
            cpu,
            state: SpinNoIrq::new(StallState {
                interval_start: None,
                last_warning: None,
            }),
        }
    }

    /// Publish only membership transitions or execution progress, under partition
    /// protection. Additional enqueues must not reset an existing stall interval.
    pub(crate) fn publish_interval(&self, start: Option<MonotonicInstant>) {
        let mut state = self.state.lock();
        state.interval_start = start;
        state.last_warning = None;
    }

    /// Sample once without waiting for a producer. Busy is unavailable, not healthy.
    /// A new episode clears throttling; repeated warnings are at least one threshold
    /// apart. The returned duration is an observation, not a current-state guarantee.
    pub(super) fn take_warning(
        &self,
        now: MonotonicInstant,
        threshold: TimeSpan,
    ) -> Option<TimeSpan> {
        if threshold.is_zero() {
            return None;
        }
        let mut state = self.state.try_lock()?;
        let elapsed = now.checked_duration_since(state.interval_start?)?;
        if elapsed < threshold {
            return None;
        }
        if let Some(last) = state.last_warning
            && now.checked_duration_since(last)? < threshold
        {
            return None;
        }
        state.last_warning = Some(now);
        Some(elapsed)
    }

    #[cfg(unittest)]
    pub(super) fn interval_start(&self) -> Option<MonotonicInstant> {
        self.state.lock().interval_start
    }
}

unittest::path_mod_test!(
    record_unittest,
    "../../ut-tests/diagnostics_record_unittest.rs"
);
