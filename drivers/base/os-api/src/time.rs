// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver time capability.

use ktime_types::MonotonicInstant;

/// Monotonic clock capability for drivers.
///
/// A host kernel implements this and passes it to its driver framework when a
/// driver needs bounded polling, timeout accounting, or delay loops without
/// depending on the host kernel's clock API directly.
pub trait TimeOp: Sync {
    /// Return the current monotonic time.
    fn monotonic_time(&self) -> MonotonicInstant;
}
