// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Queue ownership, immutable configuration and common admission control.

use alloc::sync::{Arc, Weak};
use core::{mem::MaybeUninit, num::NonZeroUsize, sync::atomic::AtomicUsize, task::Waker};

use kalloc_types::{AllocFlags, KBox};
use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use ksync::Mutex;
use kworkerpool_ng::PartitionDispatch;

mod api;
mod color;
mod config;
mod control;
mod delayed;
mod execution;
mod flush;
mod freeze;
pub(crate) mod partition;
mod routing;
mod shutdown;
mod source;
mod submission;

use color::ColorCompletion;
pub use config::{CpuPolicy, QueueConfig, QueueConfigError};
use control::COLOR_COUNT;
pub use freeze::{FrozenQueue, QueueFreezeCancellation, QueueFreezeError};
use partition::QueuePartition;
pub use source::QueuePartitionSource;
use source::QueueSource;
pub use submission::SubmitError;

/// Owns a validated configuration and uninitialized partition storage.
/// Dropping this value never binds providers or stops workers.
pub struct PreparedQueue {
    config: QueueConfig,
    storage: KBox<[MaybeUninit<Arc<QueuePartition>>]>,
}

impl PreparedQueue {
    /// Reserves partition storage in reclaim-safe, sleepable task context.
    /// Hold no locks needed by reclaim or I/O, and keep IRQs/preemption enabled.
    /// The caller must be outside non-sleepable RCU scopes.
    ///
    /// # Errors
    /// Returns NoMemory on allocation failure or InvalidInput on layout overflow,
    /// without binding any execution provider.
    pub fn new(config: QueueConfig) -> KResult<Self> {
        Self::new_with_flags(config, AllocFlags::KERNEL)
    }

    /// Reserves storage under the selected allocation permissions.
    /// The caller must satisfy the flags' context contract. NOWAIT does not
    /// make subsequent provider binding IRQ-safe or grant NMI safety.
    ///
    /// # Errors
    /// Returns InvalidInput for unknown flags or layout overflow, and NoMemory
    /// on allocation failure. Errors consume the config without binding providers.
    pub fn new_with_flags(config: QueueConfig, flags: AllocFlags) -> KResult<Self> {
        let storage = KBox::new_uninit_slice_with_flags(config.cpus().len(), flags)?;
        Ok(Self { config, storage })
    }
}

struct InitializedPrefix<'a, T> {
    storage: &'a mut [MaybeUninit<T>],
    initialized: usize,
}

impl<T> InitializedPrefix<'_, T> {
    fn push(&mut self, value: T) {
        self.storage[self.initialized].write(value);
        self.initialized += 1;
    }
}

impl<T> Drop for InitializedPrefix<'_, T> {
    fn drop(&mut self) {
        for slot in &mut self.storage[..self.initialized] {
            // SAFETY: push initializes each prefix slot exactly once before
            // advancing the count; this guard holds exclusive storage access.
            unsafe { slot.assume_init_drop() };
        }
    }
}

fn initialize_storage<T>(
    mut storage: KBox<[MaybeUninit<T>]>,
    mut initialize: impl FnMut(usize) -> T,
) -> KBox<[T]> {
    let length = storage.len();
    {
        let mut prefix = InitializedPrefix {
            storage: &mut storage,
            initialized: 0,
        };
        for index in 0..length {
            prefix.push(initialize(index));
        }
        prefix.initialized = 0;
    }
    // SAFETY: every slot was initialized above, and the prefix guard was
    // disarmed only after completion. Ownership transfers without another drop.
    unsafe { storage.assume_init() }
}

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
    partitions: KBox<[Arc<QueuePartition>]>,
    control: AtomicUsize,
    next_cpu_index: AtomicUsize,
    // Serializes cutoff/setup, never admission, completion or shutdown.
    sealed_colors: Mutex<[Option<Arc<ColorCompletion>>; COLOR_COUNT]>,
    color_progress: Arc<kcompletion::WaitQueue>,
    // Owns a reversible partition-dispatch pause, independently of admission.
    is_freezing: kspin::SpinNoIrq<bool>,
}

impl Queue {
    /// Returns immutable routing and per-CPU release limits chosen at preparation.
    pub fn config(&self) -> &QueueConfig {
        &self.config
    }

    /// Prepares unbound management for tests that drive claims manually.
    #[cfg(unittest)]
    pub(crate) fn new(config: QueueConfig) -> Arc<Self> {
        let prepared = PreparedQueue::new_with_flags(config, AllocFlags::NOWAIT).unwrap();
        Self::prepare(prepared, |_, _| None, |partition| partition)
    }

    #[cfg(all(unittest, feature = "stall-check"))]
    pub(crate) fn with_diagnostics_for_test(
        cpus: &[LogicalCpuId],
        max_active: usize,
        mut register: impl FnMut(LogicalCpuId) -> Arc<crate::diagnostics::StallRecord>,
    ) -> Arc<Self> {
        let config = QueueConfig::validate(
            cpus,
            CpuPolicy::RoundRobin,
            max_active,
            kalloc_types::AllocFlags::NOWAIT,
            |_| true,
        )
        .unwrap();
        Self::prepare(
            PreparedQueue::new_with_flags(config, AllocFlags::NOWAIT).unwrap(),
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
        prepared: PreparedQueue,
        mut service: impl FnMut(
            LogicalCpuId,
        ) -> (Arc<PartitionDispatch<QueuePartitionSource>>, Arc<Waker>),
    ) -> Arc<Self> {
        Self::prepare(
            prepared,
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
    /// Names are copied inline during preparation as UTF-8 prefixes of at most
    /// 31 bytes. They are diagnostic labels, not unique queue identifiers.
    ///
    /// # Panics
    /// Propagates provider closure panics and fatal kernel allocation failures.
    #[cfg(feature = "stall-check")]
    pub(crate) fn with_dispatch_and_diagnostics(
        prepared: PreparedQueue,
        name: &str,
        directory: &crate::diagnostics::Directory,
        mut service: impl FnMut(
            LogicalCpuId,
        ) -> (Arc<PartitionDispatch<QueuePartitionSource>>, Arc<Waker>),
    ) -> Arc<Self> {
        Self::prepare(
            prepared,
            |queue, cpu| {
                let (dispatch, kick) = service(cpu);
                Some(QueueSource::new(queue.clone(), cpu, dispatch, kick))
            },
            |partition| {
                let record = directory.register(name, partition.cpu_id());
                partition.with_diagnostic(record)
            },
        )
    }

    fn prepare(
        prepared: PreparedQueue,
        mut source: impl FnMut(&Weak<Self>, LogicalCpuId) -> Option<QueueSource>,
        mut decorate: impl FnMut(QueuePartition) -> QueuePartition,
    ) -> Arc<Self> {
        let PreparedQueue { config, storage } = prepared;
        Arc::new_cyclic(|queue| Self {
            partitions: initialize_storage(storage, |index| {
                let cpu = config.cpus()[index];
                Arc::new(decorate(QueuePartition::with_source(
                    cpu,
                    NonZeroUsize::new(config.max_active()).expect("validated positive allowance"),
                    source(queue, cpu),
                )))
            }),
            config,
            control: AtomicUsize::new(0),
            next_cpu_index: AtomicUsize::new(0),
            sealed_colors: Mutex::new(core::array::from_fn(|_| None)),
            color_progress: Arc::new(kcompletion::WaitQueue::new()),
            is_freezing: kspin::SpinNoIrq::new(false),
        })
    }

    /// Resolves only allowed CPUs without allocating or scanning every partition.
    pub(crate) fn partition(&self, cpu_id: LogicalCpuId) -> Option<&Arc<QueuePartition>> {
        let index = self.config.cpus().binary_search(&cpu_id).ok()?;
        Some(&self.partitions[index])
    }
}

unittest::path_mod_test!(pub(crate) mod queue_unittest, "../ut-tests/queue_unittest.rs");
