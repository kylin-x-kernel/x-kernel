// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable queue configuration and validation.

use alloc::boxed::Box;
use core::num::NonZeroUsize;

use kcpu_id_map::LogicalCpuId;

/// Automatic selection for a task with no unfinished queue association.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuPolicy {
    /// Sample the submitting CPU at selection; never fall back to another CPU.
    CurrentCpu,
    /// Visit allowed CPUs in ascending order, advancing only on acceptance.
    RoundRobin,
}

/// Invalid immutable queue configuration; no queue has been created.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QueueConfigError {
    /// At least one allowed CPU is required.
    EmptyCpuSet,
    /// The same CPU was supplied more than once.
    DuplicateCpu(LogicalCpuId),
    /// The CPU has no mapping in the initialized boot CPU set.
    CpuUnavailable(LogicalCpuId),
    /// Zero is neither a default nor unlimited release allowance.
    ZeroMaxActive,
}

/// Immutable CPU selection and per-CPU release limit for a future queue.
///
/// This is configuration, not an initialized service: constructing it does not
/// register sources or diagnostics, create workers, or accept tasks. A future
/// queue constructor must also check execution-provider readiness and category.
#[derive(Debug)]
pub struct QueueConfig {
    cpus: Box<[LogicalCpuId]>,
    policy: CpuPolicy,
    max_active: NonZeroUsize,
}

impl QueueConfig {
    /// Validates an explicit policy, nonempty CPU set and positive release limit.
    ///
    /// Call in allocation-capable task context after boot CPU-map initialization.
    /// CPUs must be distinct and present in that map; their input order does not
    /// affect routing. No hotplug or runtime configuration changes are supported.
    /// Allocation follows the kernel allocator policy, with no recoverable OOM
    /// result. Invalid input returns an error without exposing configuration.
    ///
    /// # Errors
    /// Rejects an empty CPU set, duplicate or unavailable CPUs, and zero
    /// `max_active`. When several inputs are invalid, error priority is unspecified.
    ///
    /// # Examples
    ///
    /// ```no_run
    /// # fn example() -> Result<(), kworkqueue_ng::QueueConfigError> {
    /// use kworkqueue_ng::{CpuPolicy, QueueConfig};
    /// // After boot CPU-map initialization, when logical CPU 0 is present.
    /// let config = QueueConfig::new(&[0.into()], CpuPolicy::CurrentCpu, 1)?;
    /// assert_eq!(config.max_active(), 1);
    /// # Ok(())
    /// # }
    /// ```
    pub fn new(
        cpus: &[LogicalCpuId],
        policy: CpuPolicy,
        max_active: usize,
    ) -> Result<Self, QueueConfigError> {
        Self::validate(cpus, policy, max_active, |cpu| {
            kcpu_id_map::raw_cpu_id(cpu).is_some()
        })
    }

    pub(super) fn validate(
        cpus: &[LogicalCpuId],
        policy: CpuPolicy,
        max_active: usize,
        is_present: impl Fn(LogicalCpuId) -> bool,
    ) -> Result<Self, QueueConfigError> {
        let max_active = NonZeroUsize::new(max_active).ok_or(QueueConfigError::ZeroMaxActive)?;
        if cpus.is_empty() {
            return Err(QueueConfigError::EmptyCpuSet);
        }
        for &cpu in cpus {
            if !is_present(cpu) {
                return Err(QueueConfigError::CpuUnavailable(cpu));
            }
        }
        let mut cpus = cpus.to_vec();
        cpus.sort_unstable();
        for pair in cpus.windows(2) {
            if pair[0] == pair[1] {
                return Err(QueueConfigError::DuplicateCpu(pair[0]));
            }
        }
        Ok(Self {
            cpus: cpus.into_boxed_slice(),
            policy,
            max_active,
        })
    }

    /// Returns the allowed CPUs in ascending numeric order.
    pub fn cpus(&self) -> &[LogicalCpuId] {
        &self.cpus
    }

    /// Returns the explicitly chosen automatic selection policy.
    pub fn policy(&self) -> CpuPolicy {
        self.policy
    }

    /// Returns the fixed release allowance applied independently on each CPU.
    ///
    /// This is not a worker count or a limit on accepted waiting submissions.
    pub fn max_active(&self) -> usize {
        self.max_active.get()
    }
}
