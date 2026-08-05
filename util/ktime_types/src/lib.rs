// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Semantic time value types for X-Kernel.
//!
//! [`TimeSpan`] represents a non-negative length of time, [`Instant`] represents
//! a point in a specific clock domain, and [`SystemTime`] represents a signed
//! Unix timestamp. The distinct types prevent durations, deadlines, and wall
//! clock timestamps from being mixed accidentally.

#![no_std]
#![deny(unsafe_code)]

mod instant;
mod span;
mod system_time;
mod units;

pub use instant::{
    Boottime, BoottimeInstant, Instant, Monotonic, MonotonicInstant, ProcessCpu, ProcessCpuInstant,
    ThreadCpu, ThreadCpuInstant,
};
pub use span::TimeSpan;
pub use system_time::{SystemTime, SystemTimeError};
pub use units::{
    MICROS_PER_SEC, MILLIS_PER_SEC, NANOS_PER_MICROS, NANOS_PER_MILLIS, NANOS_PER_SEC,
};

#[cfg(unittest)]
mod tests;
