// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Immutable queue configuration and validation.

use core::num::NonZeroUsize;

use kalloc_types::{AllocFlags, KBox};
use kcpu_id_map::LogicalCpuId;
use kerrno::{KError, KResult};

/// Automatic selection for a task with no unfinished queue association.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuPolicy {
    /// Sample the submitting CPU at selection; never fall back to another CPU.
    CurrentCpu,
    /// Visit allowed CPUs in ascending order, advancing only on acceptance.
    RoundRobin,
}

/// Configuration validation or storage preparation failed; no queue was created.
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
    /// CPU selection storage could not be prepared with the requested permissions.
    Allocation(KError),
}

impl From<KError> for QueueConfigError {
    fn from(error: KError) -> Self {
        Self::Allocation(error)
    }
}

/// Immutable CPU selection and per-CPU release limit for a future queue.
///
/// This is configuration, not an initialized service: constructing it does not
/// register sources or diagnostics, create workers, or accept tasks. A future
/// queue constructor must also check execution-provider readiness and category.
#[derive(Debug)]
pub struct QueueConfig {
    cpus: KBox<[LogicalCpuId]>,
    policy: CpuPolicy,
    max_active: NonZeroUsize,
}

impl QueueConfig {
    /// Validates an explicit policy, nonempty CPU set and positive release limit.
    ///
    /// Call in sleepable, reclaim-safe task context after boot CPU-map initialization,
    /// with IRQs/preemption enabled and no locks needed by reclaim, IO or filesystem
    /// entry, outside non-sleepable RCU scopes. CPU storage uses KERNEL permissions
    /// before any queue publication.
    /// CPUs must be distinct and present in that map; their input order does not
    /// affect routing. No hotplug or runtime configuration changes are supported.
    /// Invalid input or allocation failure returns an error without exposing configuration.
    ///
    /// # Errors
    /// Rejects an empty CPU set, duplicate or unavailable CPUs, and zero
    /// `max_active`. Storage failure returns Allocation(NoMemory). Reservation
    /// precedes sorting, so OOM may precede DuplicateCpu. Other error priorities
    /// among invalid inputs are unspecified.
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
        Self::new_with_flags(cpus, policy, max_active, AllocFlags::KERNEL)
    }

    /// Prepares configuration with explicit CPU-storage allocation permissions.
    ///
    /// CPU-map prerequisites and validation match [`Self::new`]. NOWAIT avoids
    /// sleeping reclaim for this allocation only; it promises no reserve, bounded
    /// latency or NMI safety. NOIO/NOFS still permit sleeping. Callers must satisfy
    /// the selected allocator's context and lock-order contract. This does not
    /// relax the separate worker/queue construction context requirements.
    ///
    /// # Errors
    /// Validation errors match [`Self::new`]. Unknown flag bits return
    /// Allocation(InvalidInput) when storage preparation is reached.
    pub fn new_with_flags(
        cpus: &[LogicalCpuId],
        policy: CpuPolicy,
        max_active: usize,
        flags: AllocFlags,
    ) -> Result<Self, QueueConfigError> {
        Self::validate(cpus, policy, max_active, flags, |cpu| {
            kcpu_id_map::raw_cpu_id(cpu).is_some()
        })
    }

    pub(super) fn validate(
        cpus: &[LogicalCpuId],
        policy: CpuPolicy,
        max_active: usize,
        flags: AllocFlags,
        is_present: impl Fn(LogicalCpuId) -> bool,
    ) -> Result<Self, QueueConfigError> {
        Self::validate_with_allocator(cpus, policy, max_active, is_present, |cpus| {
            KBox::from_slice_with_flags(cpus, flags)
        })
    }

    pub(super) fn validate_with_allocator(
        cpus: &[LogicalCpuId],
        policy: CpuPolicy,
        max_active: usize,
        is_present: impl Fn(LogicalCpuId) -> bool,
        allocate: impl FnOnce(&[LogicalCpuId]) -> KResult<KBox<[LogicalCpuId]>>,
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
        let mut cpus = allocate(cpus)?;
        cpus.sort_unstable();
        for pair in cpus.windows(2) {
            if pair[0] == pair[1] {
                return Err(QueueConfigError::DuplicateCpu(pair[0]));
            }
        }
        Ok(Self {
            cpus,
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
