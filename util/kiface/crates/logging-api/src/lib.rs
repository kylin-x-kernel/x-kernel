// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Log sink and initialization-aware log-context observations.

#![no_std]

/// Ordinary logging sink; may run under a spinlock, must not sleep or recursively log.
#[kiface::interface]
pub trait LogSinkIf {
    /// Writes a formatted record through the ordinary logging destination.
    fn write_str(s: &str);
}

/// Initialization-aware observations used to format a log record.
/// Identity may be absent before boot services are ready; queries must not recurse into logging.
#[kiface::interface]
pub trait LogContextIf {
    /// Samples elapsed time in the runtime's log clock domain.
    fn now() -> ktime_types::TimeSpan;
    /// Samples an optional logical CPU identity without pinning the caller.
    fn cpu_id() -> Option<usize>;
    /// Samples an optional task identity without creating a task reference.
    fn task_id() -> Option<u64>;
}
