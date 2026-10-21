// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Clock identities shared by kernel subsystems.

/// Semantic clock identity, independent of platform ABI numbering.
///
/// Each operation decides which clocks it supports. This enum neither selects
/// a clock source nor implies that every variant has a distinct implementation.
/// Raw ABI parsing and subsystem-specific conversions belong to their adapters.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClockId {
    /// Wall-clock time.
    Realtime,
    /// Monotonic elapsed time excluding suspended time.
    Monotonic,
    /// CPU time consumed by a process.
    ProcessCpu,
    /// CPU time consumed by a thread.
    ThreadCpu,
    /// Unadjusted monotonic clock.
    MonotonicRaw,
    /// Coarse wall-clock time.
    RealtimeCoarse,
    /// Coarse monotonic elapsed time.
    MonotonicCoarse,
    /// Elapsed time including accounted suspended time.
    Boottime,
}
