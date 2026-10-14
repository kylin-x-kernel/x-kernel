// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Complete boot-CPU coverage owned privately until construction succeeds.

use alloc::{sync::Arc, vec::Vec};
use core::{num::NonZeroUsize, task::Waker};

use kcpu_id_map::LogicalCpuId;
use ktime_types::TimeSpan;

use super::{
    NormalService, PartitionWorkSource,
    runtime::{NormalRuntime, RuntimeError, validate_idle_interval},
};

/// Owns each CPU's runtime, not an additional copy of its CPU/readiness state.
/// A successful constructor covers the fixed boot CPU set; no partially built
/// collection escapes. Integration must retain this owner until all producers
/// stop, sources drain/detach, and stop completes. Drop does not block or stop
/// runtimes implicitly. Global registration and category selection live elsewhere.
pub struct NormalServices<S> {
    runtimes: Vec<NormalRuntime<S>>,
}

impl<S: PartitionWorkSource + Send + Sync + 'static> NormalServices<S> {
    /// Returns the matching dispatcher and prepared notification for a boot CPU.
    ///
    /// Lookup and reference cloning allocate nothing. Returns None for a CPU outside
    /// this set. The owner must keep services running until bound sources are drained
    /// and detached; a retained binding does not prevent explicit service stop.
    pub fn binding(
        &self,
        cpu: LogicalCpuId,
    ) -> Option<(Arc<crate::PartitionDispatch<S>>, Arc<Waker>)> {
        self.service(cpu)
            .map(|service| (service.dispatch.clone(), service.notification_waker()))
    }

    /// Prepares normal execution on every present boot CPU, without publishing it.
    /// Call after task/timer initialization on all those CPUs. The caller may
    /// register the returned complete set, or stop it if a later setup step fails.
    /// `limit` bounds workers per CPU; `idle_for` is the positive idle-retirement
    /// interval. Idle retirement retains one baseline worker on every CPU.
    ///
    /// # Errors
    /// Invalid context or idle interval rejects before service allocation. Empty
    /// boot coverage or a per-CPU startup error returns only after joined rollback.
    ///
    /// # Panics
    /// Propagates initialized task/timer invariants and fatal kernel allocation.
    pub fn start(limit: NonZeroUsize, idle_for: TimeSpan) -> Result<Self, RuntimeError> {
        if !ktask::is_sleepable_context() {
            return Err(RuntimeError::InvalidContext);
        }
        validate_idle_interval(idle_for)?;
        let mut cpus = Vec::new();
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
        Self::start_with(&cpus, |cpu| NormalRuntime::start(cpu, limit, idle_for))
    }

    /// The boot map supplies unique sorted CPUs. The injected starter must retain
    /// sleepable context, return a matching runtime, and clean up its own failed
    /// attempt; it must not publish sources or any partially prepared service.
    fn start_with(
        cpus: &[LogicalCpuId],
        mut start: impl FnMut(LogicalCpuId) -> Result<NormalRuntime<S>, RuntimeError>,
    ) -> Result<Self, RuntimeError> {
        if cpus.is_empty() {
            return Err(RuntimeError::CpuUnavailable);
        }
        let mut runtimes: Vec<NormalRuntime<S>> = Vec::with_capacity(cpus.len());
        for &cpu in cpus {
            match start(cpu) {
                Ok(runtime) => runtimes.push(runtime),
                Err(error) => {
                    // No service was exposed, so these runtimes have no sources
                    // or business callbacks. Preserve the original startup error.
                    while let Some(runtime) = runtimes.pop() {
                        runtime.stop().expect("startup rollback remains sleepable");
                    }
                    return Err(error);
                }
            }
        }
        Ok(Self { runtimes })
    }

    /// Resolves exact boot CPU identity without allocating or inventing a fallback.
    pub(super) fn service(&self, cpu: LogicalCpuId) -> Option<&NormalService<S>> {
        self.runtimes
            .binary_search_by_key(&cpu, |runtime| runtime.service.cpu)
            .ok()
            .map(|index| self.runtimes[index].service.as_ref())
    }

    /// Joins all services in reverse construction order after source teardown.
    /// Not Queue shutdown: caller must stop producers and drain/detach every source
    /// first, and must not execute this operation inside an owned worker/manager.
    /// Repeated calls after complete stop are harmless. References alone do not
    /// authorize publishing new demand during or after this operation.
    ///
    /// # Errors
    /// Invalid context returns before stopping any CPU service.
    ///
    /// # Panics
    /// Propagates unexpected nonzero manager exits.
    pub fn stop(&self) -> Result<(), RuntimeError> {
        if !ktask::is_sleepable_context() {
            return Err(RuntimeError::InvalidContext);
        }
        for runtime in self.runtimes.iter().rev() {
            runtime.stop().expect("joined teardown remains sleepable");
        }
        Ok(())
    }
}

unittest::path_mod_test!(
    services_unittest,
    "../../ut-tests/normal_services_unittest.rs"
);
