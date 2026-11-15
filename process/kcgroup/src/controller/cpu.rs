// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `cpu` controller: CPU weight and bandwidth parameters.
//!
//! `cpu.weight` is enforced immediately: a write re-applies the translated
//! nice value to every member task through the scheduler hook, so relative
//! shares change without a reload. `cpu.max` stores its values in a
//! [`kbandwidth::Bandwidth`] state machine; the scheduler charges per-task
//! runtime into it every tick and collapses member weights to the minimum
//! while a throttle window is open (see `ktask::run_queue::bandwidth`).

use core::sync::atomic::{AtomicU64, Ordering};

use kerrno::{KError, KResult, LinuxError};

use super::{Controller, ControllerKind};

/// Largest numeric `cpu.weight` value accepted by Linux.
pub(crate) const CPU_WEIGHT_MAX: u32 = 10_000;

/// Per-cgroup `cpu` controller state.
pub struct CpuController {
    /// Relative CPU share, 1..=10000, Linux default 100.
    weight: AtomicU64,
    /// Bandwidth accounting shared with the scheduler's charge path through
    /// the dependency-free `kbandwidth` state machine.
    bandwidth: alloc::sync::Arc<kbandwidth::Bandwidth>,
}

impl Default for CpuController {
    fn default() -> Self {
        Self::new()
    }
}

impl CpuController {
    /// Creates an active controller with Linux defaults.
    pub fn new() -> Self {
        Self {
            weight: AtomicU64::new(100),
            bandwidth: alloc::sync::Arc::new(kbandwidth::Bandwidth::new()),
        }
    }

    /// Returns the relative CPU share.
    pub fn weight(&self) -> u32 {
        self.weight.load(Ordering::Acquire) as u32
    }

    /// Stores the relative CPU share.
    ///
    /// # Errors
    ///
    /// Returns `ERANGE` outside the Linux `cpu.weight` range 1..=10000.
    pub fn set_weight(&self, weight: u32) -> KResult<()> {
        if weight == 0 || weight > CPU_WEIGHT_MAX {
            return Err(KError::from(LinuxError::ERANGE));
        }
        self.weight.store(weight as u64, Ordering::Release);
        Ok(())
    }

    /// Returns the live bandwidth tracker this controller owns.
    pub fn bandwidth(&self) -> alloc::sync::Arc<kbandwidth::Bandwidth> {
        self.bandwidth.clone()
    }

    /// Returns the bandwidth quota, or `None` for "max".
    pub fn quota(&self) -> Option<u64> {
        self.bandwidth.quota_us()
    }

    /// Returns the bandwidth period.
    pub fn period(&self) -> u64 {
        self.bandwidth.period_us()
    }

    /// Stores the bandwidth limit.
    ///
    /// `quota` of `None` means "max". The pair follows Linux's `cpu.max`
    /// validation: the period stays inside 1ms..1s and a numeric quota is at
    /// least 1ms.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` for out-of-range values.
    pub fn set_bandwidth(&self, quota: Option<u64>, period: u64) -> KResult<()> {
        let is_valid = (kbandwidth::PERIOD_MIN_US..=kbandwidth::PERIOD_MAX_US).contains(&period)
            && quota.is_none_or(|value| value >= kbandwidth::QUOTA_MIN_US);
        if !is_valid {
            return Err(KError::from(LinuxError::EINVAL));
        }
        if self.bandwidth.is_throttled() {
            crate::release_bandwidth(&self.bandwidth);
            self.bandwidth.reopen();
        }
        let _ = self.bandwidth.set(quota, period);
        Ok(())
    }

    /// Returns the bandwidth counters reported by `cpu.stat`.
    pub fn bandwidth_counters(&self) -> (u64, u64, u64) {
        self.bandwidth.counters()
    }

    /// Returns the cumulative charged runtime in microseconds.
    pub fn total_usage_us(&self) -> u64 {
        self.bandwidth.total_usage_us()
    }
}

impl Controller for CpuController {
    fn kind(&self) -> ControllerKind {
        ControllerKind::Cpu
    }

    // Weight is the enforcement channel this controller owns: writes are
    // pushed to member tasks by the filesystem adapter, exactly like the
    // cpuset masks. Bandwidth runtime arrives from the scheduler tick path,
    // which charges `bandwidth()` directly.
}
