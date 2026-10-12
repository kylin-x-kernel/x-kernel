// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime scheduler knobs. Fair, RT, and deadline values come from Kconfig.

const US_TO_NS: u64 = 1_000;

const fn us_to_ns(us: usize) -> u64 {
    us as u64 * US_TO_NS
}

const fn clamp_runtime_ns(runtime_us: usize, period_us: usize) -> u64 {
    let period = us_to_ns(period_us);
    let runtime = us_to_ns(runtime_us);
    if runtime > period { period } else { runtime }
}

/// Default EEVDF request length (`SCHED_FAIR_REQUEST_US`).
pub const DEFAULT_SLICE_NS: u64 = us_to_ns(kbuild_config::SCHED_FAIR_REQUEST_US);

/// Default `SCHED_RR` quantum (`SCHED_RR_SLICE_US`).
pub const DEFAULT_RR_SLICE_NS: u64 = us_to_ns(kbuild_config::SCHED_RR_SLICE_US);

/// Default RT bandwidth period (`SCHED_RT_PERIOD_US`).
pub const DEFAULT_RT_PERIOD_NS: u64 = us_to_ns(kbuild_config::SCHED_RT_PERIOD_US);

/// Default RT bandwidth runtime per period (`SCHED_RT_RUNTIME_US`).
///
/// Runtime greater than the period is clamped to the period.
pub const DEFAULT_RT_RUNTIME_NS: u64 = clamp_runtime_ns(
    kbuild_config::SCHED_RT_RUNTIME_US,
    kbuild_config::SCHED_RT_PERIOD_US,
);

/// Lowest valid `SCHED_FIFO` / `SCHED_RR` priority.
pub(crate) const MIN_RT_PRIORITY: u8 = 1;

/// Highest valid `SCHED_FIFO` / `SCHED_RR` priority.
pub(crate) const MAX_RT_PRIORITY: u8 = 99;

/// Number of user-visible RT priority levels (`MIN_RT_PRIORITY..=MAX_RT_PRIORITY`).
pub(crate) const RT_PRIO_LEVELS: usize = (MAX_RT_PRIORITY - MIN_RT_PRIORITY) as usize + 1;

/// Smallest deadline runtime a task may request (`SCHED_DL_MIN_RUNTIME_US`).
pub(crate) const MIN_DL_RUNTIME_NS: u64 = us_to_ns(kbuild_config::SCHED_DL_MIN_RUNTIME_US);

/// Bandwidth kept unassigned on each run queue (`SCHED_DL_BW_MARGIN_PCT`).
///
/// Admission rejects a deadline task when the run queue's assigned bandwidth
/// would exceed `1_000_000 - margin` parts per million of one CPU.
pub(crate) const DL_BW_MARGIN_PPM: u64 = kbuild_config::SCHED_DL_BW_MARGIN_PCT as u64 * 10_000;

const _: () = {
    assert!(MIN_RT_PRIORITY >= 1);
    assert!(MAX_RT_PRIORITY >= MIN_RT_PRIORITY);
    // `idx` shifts a `u128` bitmap by `priority - MIN_RT_PRIORITY`.
    assert!(RT_PRIO_LEVELS <= 128);
    // Admission compares against `1_000_000 - DL_BW_MARGIN_PPM`.
    assert!(DL_BW_MARGIN_PPM < 1_000_000);
};
