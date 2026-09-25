// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue ownership, immutable configuration and common admission control.

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
};
use core::{num::NonZeroUsize, sync::atomic::AtomicUsize, task::Waker};

use kcpu_id_map::LogicalCpuId;
use ksync::Mutex;
use kworkerpool_ng::PartitionDispatch;

mod api;
mod color;
mod config;
mod control;
mod delayed;
mod execution;
mod flush;
pub(crate) mod partition;
mod routing;
mod shutdown;
mod source;
mod submission;

use color::ColorCompletion;
pub use config::{CpuPolicy, QueueConfig, QueueConfigError};
use control::COLOR_COUNT;
use partition::QueuePartition;
pub use source::QueuePartitionSource;
use source::QueueSource;
pub use submission::SubmitError;

/// Owns one partition per logical CPU and the queue-wide admission boundary.
///
/// Queue owns partitions; partitions never own Queue. Accepted Work occurrences
/// retain Queue until cleanup, then their independent partition reference covers
/// the final count retirement. Neither reference owns the externally stored Work.
/// Caller-owned queues may flush or close_and_drain their ordinary work without
/// stopping shared execution providers. Before shutdown, stop delayed producers
/// and synchronously cancel all associated DelayedWork, even logically idle ones.
/// Dropping a queue reference does not perform implicit synchronous shutdown.
pub struct Queue {
    config: QueueConfig,
    // Same compact order as config.cpus(); disallowed CPUs own no partition.
    partitions: Box<[Arc<QueuePartition>]>,
    control: AtomicUsize,
    next_cpu_index: AtomicUsize,
    // Serializes cutoff/setup, never admission, completion or shutdown.
    sealed_colors: Mutex<[Option<Arc<ColorCompletion>>; COLOR_COUNT]>,
}

impl Queue {
    /// Returns immutable routing and per-CPU release limits chosen at preparation.
    pub fn config(&self) -> &QueueConfig {
        &self.config
    }

    /// Prepares unbound management for tests that drive claims manually.
    #[cfg(unittest)]
    pub(crate) fn new(config: QueueConfig) -> Arc<Self> {
        Self::prepare(config, |_, _| None, |partition| partition)
    }

    #[cfg(all(unittest, feature = "stall-check"))]
    pub(crate) fn with_diagnostics_for_test(
        cpus: &[LogicalCpuId],
        max_active: usize,
        mut register: impl FnMut(LogicalCpuId) -> Arc<crate::diagnostics::StallRecord>,
    ) -> Arc<Self> {
        let config =
            QueueConfig::validate(cpus, CpuPolicy::RoundRobin, max_active, |_| true).unwrap();
        Self::prepare(
            config,
            |_, _| None,
            |partition| {
                let record = register(partition.cpu_id());
                partition.with_diagnostic(record)
            },
        )
    }

    /// Bind every allowed CPU before exposure. Providers supply matching CPU/category
    /// services and IRQ-compatible kicks that retain no Queue/Work/registration.
    /// This is internal construction, not runtime readiness or public initialization.
    /// Call in sleepable task context. Retain live providers until ordinary drain
    /// and source detachment finish; Queue does not start or stop execution services.
    ///
    /// # Panics
    /// Propagates provider closure panics and fatal kernel allocation failures.
    pub(crate) fn with_dispatch(
        config: QueueConfig,
        mut service: impl FnMut(
            LogicalCpuId,
        ) -> (Arc<PartitionDispatch<QueuePartitionSource>>, Arc<Waker>),
    ) -> Arc<Self> {
        Self::prepare(
            config,
            |queue, cpu| {
                let (dispatch, kick) = service(cpu);
                Some(QueueSource::new(queue.clone(), cpu, dispatch, kick))
            },
            |partition| partition,
        )
    }

    /// Binds execution and named per-CPU diagnostics before exposing the queue.
    /// Has the same context/provider lifetime contract as [`Self::with_dispatch`].
    /// The directory retains only weak diagnostic records, never this queue.
    /// Names are copied during preparation; submission does not allocate them.
    ///
    /// # Panics
    /// Propagates provider closure panics and fatal kernel allocation failures.
    #[cfg(feature = "stall-check")]
    pub(crate) fn with_dispatch_and_diagnostics(
        config: QueueConfig,
        name: &str,
        directory: &crate::diagnostics::Directory,
        mut service: impl FnMut(
            LogicalCpuId,
        ) -> (Arc<PartitionDispatch<QueuePartitionSource>>, Arc<Waker>),
    ) -> Arc<Self> {
        Self::prepare(
            config,
            |queue, cpu| {
                let (dispatch, kick) = service(cpu);
                Some(QueueSource::new(queue.clone(), cpu, dispatch, kick))
            },
            |partition| {
                let record = directory.register(name.into(), partition.cpu_id());
                partition.with_diagnostic(record)
            },
        )
    }

    fn prepare(
        config: QueueConfig,
        mut source: impl FnMut(&Weak<Self>, LogicalCpuId) -> Option<QueueSource>,
        mut decorate: impl FnMut(QueuePartition) -> QueuePartition,
    ) -> Arc<Self> {
        Arc::new_cyclic(|queue| Self {
            partitions: config
                .cpus()
                .iter()
                .map(|&cpu| {
                    Arc::new(decorate(QueuePartition::with_source(
                        cpu,
                        NonZeroUsize::new(config.max_active())
                            .expect("validated positive allowance"),
                        source(queue, cpu),
                    )))
                })
                .collect(),
            config,
            control: AtomicUsize::new(0),
            next_cpu_index: AtomicUsize::new(0),
            sealed_colors: Mutex::new(core::array::from_fn(|_| None)),
        })
    }

    /// Resolves only allowed CPUs without allocating or scanning every partition.
    pub(crate) fn partition(&self, cpu_id: LogicalCpuId) -> Option<&Arc<QueuePartition>> {
        let index = self.config.cpus().binary_search(&cpu_id).ok()?;
        Some(&self.partitions[index])
    }
}

#[cfg(unittest)]
pub(crate) mod tests;
