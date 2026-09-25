// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Detached boot-CPU BH services, before global action registration.

use alloc::vec::Vec;
use core::num::NonZeroUsize;

use kcpu_id_map::LogicalCpuId;

use super::{BottomHalfPriority, BottomHalfService};

/// Preparation failures before any action or source is published.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PrepareError {
    /// Preparation requires a sleepable task context.
    InvalidContext,
    /// The boot CPU set is empty or a target CPU is unavailable.
    CpuUnavailable,
}

/// Owns complete normal/high pairs for the immutable, sorted boot CPU set.
/// No partial set escapes; detached preparation can be dropped without a drain.
/// Before later publication, integration must establish handler ownership and
/// retain these services through all producers, sources and active BH passes.
/// This collection neither installs actions nor declares global readiness.
pub struct BottomHalfServices<S> {
    cpus: Vec<[BottomHalfService<S>; 2]>,
}

impl<S> BottomHalfServices<S> {
    /// Prepares both priorities on every present CPU in sleepable task context.
    /// All storage is private until success; failure drops every completed pair
    /// and any first-priority service from the currently incomplete pair.
    ///
    /// # Errors
    /// Rejects invalid context, empty CPU coverage or unavailable target CPUs.
    pub fn prepare(budget: NonZeroUsize) -> Result<Self, PrepareError> {
        if !ktask::is_sleepable_context() {
            return Err(PrepareError::InvalidContext);
        }
        let mut cpus = Vec::new();
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
        Self::prepare_with(&cpus, |cpu, priority| {
            BottomHalfService::new(cpu, priority, budget)
        })
    }

    /// The boot map supplies unique sorted CPUs. The factory must return the
    /// matching detached service and must not publish handlers, sources or kicks.
    fn prepare_with(
        cpus: &[LogicalCpuId],
        mut prepare: impl FnMut(
            LogicalCpuId,
            BottomHalfPriority,
        ) -> Result<BottomHalfService<S>, PrepareError>,
    ) -> Result<Self, PrepareError> {
        if cpus.is_empty() {
            return Err(PrepareError::CpuUnavailable);
        }
        let mut services = Vec::with_capacity(cpus.len());
        for &cpu in cpus {
            let normal = prepare(cpu, BottomHalfPriority::Normal)?;
            let high = prepare(cpu, BottomHalfPriority::High)?;
            services.push([normal, high]);
        }
        Ok(Self { cpus: services })
    }

    /// Borrows exactly one CPU/category without allocating or falling back.
    pub fn service(
        &self,
        cpu: LogicalCpuId,
        priority: BottomHalfPriority,
    ) -> Option<&BottomHalfService<S>> {
        let index = self
            .cpus
            .binary_search_by_key(&cpu, |[normal, _]| normal.cpu)
            .ok()?;
        let [normal, high] = &self.cpus[index];
        Some(match priority {
            BottomHalfPriority::Normal => normal,
            BottomHalfPriority::High => high,
        })
    }
}

#[cfg(unittest)]
mod tests;
