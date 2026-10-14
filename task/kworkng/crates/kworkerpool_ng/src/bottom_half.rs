// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! CPU-bound bounded BH execution and retained continuation requests.
//!
//! Management retains demand before issuing the independent service kick. At a
//! budget exit this service requests another opportunity if demand remains. An
//! enqueue racing an empty exit supplies its own kick. The provider must retain
//! requests made during a pass, serialize all passes on the selected CPU and
//! never call run_pass inline from its Waker. No normal worker thread is involved.

use alloc::sync::Arc;
use core::{num::NonZeroUsize, task::Waker};

use kcpu_id_map::LogicalCpuId;

use crate::{PartitionDispatch, PartitionWorkSource};

mod services;
pub use services::{BottomHalfServices, PrepareError as BottomHalfPrepareError};
unittest::path_mod_test!(bottom_half_unittest, "../ut-tests/bottom_half_unittest.rs");
mod wake;

/// The two independently serviced kernel BH categories.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BottomHalfPriority {
    /// Ordinary BH work, delivered through kirq's Tasklet vector.
    Normal,
    /// High-priority BH work, delivered through kirq's High vector.
    High,
}

/// One CPU's BH dispatcher, turn budget and independent continuation capability.
///
/// Preparation binds allocation-free local/remote notification but does not
/// register a global softirq action or create workers. Providers bind one shared
/// service per CPU/category and retain its entry while queues can use it.
pub struct BottomHalfService<S> {
    cpu: LogicalCpuId,
    dispatch: Arc<PartitionDispatch<S>>,
    budget: NonZeroUsize,
    kick: Arc<Waker>,
}

/// Rejection before any source selection or callback effect.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum BottomHalfRunError {
    /// Task, BH-disabled task or hardirq context cannot execute BH callbacks.
    InvalidContext,
    /// The softirq is running on a CPU other than this service's placement.
    WrongCpu,
}

impl<S> BottomHalfService<S> {
    /// Prepares one CPU/category dispatcher and its independent notification Waker.
    ///
    /// Call in allocation-capable task context after CPU-map initialization.
    /// Preparation allocates; later notifications and passes reuse this storage.
    /// The provider must install the matching Tasklet/High action, retain this
    /// service and ensure target CPU initialization before exposing producers.
    /// On IPI-capable systems the runtime enables fixed notifications. A non-IPI
    /// single-CPU runtime uses only the local softirq path. No hotplug is supported.
    /// This constructor does not register an action or publish global readiness.
    ///
    /// # Errors
    /// Returns [`BottomHalfPrepareError::CpuUnavailable`] if cpu is absent from the boot map.
    pub fn new(
        cpu: LogicalCpuId,
        priority: BottomHalfPriority,
        budget: NonZeroUsize,
    ) -> Result<Self, BottomHalfPrepareError> {
        let kick = wake::prepare(cpu, priority)?;
        Ok(Self {
            cpu,
            dispatch: PartitionDispatch::new(),
            budget,
            kick,
        })
    }

    /// Retains the dispatcher for a management source bound to this CPU/category.
    ///
    /// Pair with this service's notification Waker, not another service's kick.
    /// Clones an existing Arc without allocating.
    pub fn dispatch(&self) -> Arc<PartitionDispatch<S>> {
        self.dispatch.clone()
    }

    /// Retains a notification capability containing only CPU/category identity.
    ///
    /// The Waker owns no service, Queue, Work or source registration. Cloning the
    /// Arc does not allocate or call RawWaker::clone. The provider must keep its
    /// action/service alive until all producers and in-flight passes are drained.
    pub fn notification_waker(&self) -> Arc<Waker> {
        self.kick.clone()
    }

    /// Binds a prepared dispatcher to its local BH execution policy.
    ///
    /// Queue bindings must use this same dispatcher and service kick. The kick
    /// must durably request a later pass on cpu, including when called during a
    /// pass; it must be IRQ-safe and retain no Work, Queue or source registration.
    /// It must not invoke business work inline. Prepare outside spinlocked/IRQ
    /// paths; construction does not publish readiness or install a handler.
    #[cfg(unittest)]
    pub(crate) fn with_dispatch(
        cpu: LogicalCpuId,
        budget: NonZeroUsize,
        dispatch: Arc<PartitionDispatch<S>>,
        kick: Arc<Waker>,
    ) -> Self {
        Self {
            cpu,
            dispatch,
            budget,
            kick,
        }
    }
}

impl<S: PartitionWorkSource> BottomHalfService<S> {
    /// Executes a bounded pass in the provider's serial local softirq context.
    ///
    /// Holds no service/management lock across callbacks or continuation kicks.
    /// The budget counts empty source inspections too, not time or callback
    /// duration. Providers must exclude recursive/concurrent local passes and
    /// callbacks must not sleep. This check validates context, not serialization.
    ///
    /// # Errors
    /// Returns InvalidContext or WrongCpu before acquiring any Work. In particular,
    /// a hardirq interrupting softirq is rejected, not treated as a BH execution.
    ///
    /// # Panics
    /// Propagates acquisition, callback, cleanup and Waker panics. No panic recovery
    /// or implicit completion of an abandoned acquired execution is provided.
    pub fn run_pass(&self) -> Result<(), BottomHalfRunError> {
        if !kirq::context::is_softirq_context() {
            return Err(BottomHalfRunError::InvalidContext);
        }
        // kirq's softirq runner keeps preemption disabled across the entire action.
        if khal::percpu::this_cpu_id() != self.cpu {
            return Err(BottomHalfRunError::WrongCpu);
        }
        if self.dispatch.run_bounded_pass(self.budget) {
            self.kick.wake_by_ref();
        }
        Ok(())
    }
}
