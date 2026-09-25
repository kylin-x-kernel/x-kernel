// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! One normal worker's lifecycle, not Work state or scheduler runnable state.

use ktime_types::MonotonicInstant;

/// Protected by the owning normal service together with its source demand.
/// Occupied includes sleeping callbacks and resource cleanup. Only Parked owns
/// an idle timestamp; waking discards it rather than retaining stale idle age.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WorkerState {
    Empty,
    Creating,
    Prepared,
    Available,
    Selecting,
    Occupied,
    Parked { since: MonotonicInstant },
    Retiring,
}

impl WorkerState {
    pub(super) fn is_idle(&self) -> bool {
        matches!(self, Self::Available | Self::Parked { .. })
    }
}
