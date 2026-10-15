// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel Inter-Processor Communication (IPC) API
//!
//! This module provides a lightweight abstraction for CPU-to-CPU communication
//! using Inter-Processor Interrupts (IPI). It maintains per-CPU callback queues,
//! dispatches generic callbacks from the shared IPI handler, hosts the TLB
//! shootdown protocol used by page-table updates, and the system-wide CPU
//! stop protocol used before entering a platform power terminal
//! (`stop_other_cpus`).
//!
//! ## Safety
//!
//! All callbacks must be `Send` as they execute on different CPUs.

#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate log;
extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};

use kbuild_config::IPI_IRQ;
use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu, raw_cpu_id};
use khal::percpu::this_cpu_id;
use kirq::TargetCpu as IpiTarget;
use kspin::SpinNoIrq;

mod event;
mod icache;
mod notification;
mod queue;
mod stop;
pub mod tlb;

pub use event::{Callback, MulticastCallback};
#[cfg(feature = "reschedule")]
pub use notification::{RescheduleIpiIf, notify_reschedule};
#[cfg(feature = "timer-reprogram")]
pub use notification::{TimerReprogramIpiIf, notify_timer_reprogram};
pub use notification::{
    enable_fixed_notifications, notify_bottom_half, notify_highpri_bottom_half, notify_rcu,
};
use queue::IpiEventQueue;

/// Result type for IPI operations
pub type Result<T> = core::result::Result<T, KipiError>;

/// Error types for IPI operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KipiError {
    /// Invalid CPU ID (exceeds system CPU count)
    InvalidCpuId,
    /// Target CPU exists in the logical CPU map but has not initialized its local IPI queue yet.
    TargetCpuNotReady,
    /// Queue full (too many pending callbacks)
    QueueFull,
    /// Callback execution failed
    CallbackFailed,
}

impl core::fmt::Display for KipiError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::InvalidCpuId => write!(f, "Invalid CPU ID"),
            Self::TargetCpuNotReady => write!(f, "Target CPU is not ready for IPI"),
            Self::QueueFull => write!(f, "IPI queue full"),
            Self::CallbackFailed => write!(f, "Callback execution failed"),
        }
    }
}

// SAFETY: `LazyInit::new()` contains no initialized value or owned resource.
// Each CPU initializes only its own copied marker in place.
kcpu_slot::cpu_slot_cell_with_template! {
    static IPI_EVENT_QUEUE: lazyinit::LazyInit<kspin::SpinNoIrq<crate::queue::IpiEventQueue>> =
        unsafe { lazyinit::LazyInit::new() };
}

/// Shares a CPU's IPI queue (current or remote); all queue mutation is
/// serialized by the inner `SpinNoIrq`.
#[inline]
fn ipi_queue(
    cpu: LogicalCpuId,
) -> Option<&'static lazyinit::LazyInit<kspin::SpinNoIrq<crate::queue::IpiEventQueue>>> {
    let area = kcpu_slot::area(cpu)?;
    Some(IPI_EVENT_QUEUE.shared_at(area))
}

static IPI_QUEUE_READY: [AtomicBool; kbuild_config::NR_CPUS] =
    [const { AtomicBool::new(false) }; kbuild_config::NR_CPUS];

#[inline]
fn is_ipi_queue_ready(cpu_id: LogicalCpuId) -> bool {
    IPI_QUEUE_READY[cpu_id.as_usize()].load(Ordering::Acquire)
}

/// Initializes the current CPU's local IPI event queue.
///
/// This must run on every CPU before that CPU can receive callbacks through
/// [`run_on_cpu`] or either broadcast API.
///
/// # Panics
/// Panics if the current CPU's slot area has not been published yet.
pub fn init() {
    let cpu_id = this_cpu_id();
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: CPU startup publishes the current CPU \
                  slot before initializing its IPI queue"
    )]
    ipi_queue(cpu_id)
        .expect("current CPU area must be published before IPI initialization")
        .init_once(SpinNoIrq::new(IpiEventQueue::default()));
    IPI_QUEUE_READY[cpu_id.as_usize()].store(true, Ordering::Release);
}

/// Executes a callback on the specified destination CPU via IPI.
///
/// If `dest_cpu` is the current CPU, the callback runs synchronously in the
/// caller's current context. Otherwise it is queued onto the target CPU and
/// later runs from that CPU's shared IPI handler.
///
/// Callbacks must therefore be valid for the target execution context. In
/// particular, remotely executed callbacks must not rely on sleeping or other
/// process-context-only behavior unless the caller has arranged for that to be
/// safe in the target CPU's IPI handler.
///
/// # Notes
///
/// The callback must be `Send` because it may execute on a different CPU.
///
/// # Errors
///
/// Returns `KipiError::InvalidCpuId` if `dest_cpu` is outside the configured
/// range or does not belong to a present logical CPU.
///
/// Returns `KipiError::TargetCpuNotReady` if the destination CPU exists but
/// has not finished initializing its local IPI queue.
///
/// # Panics
/// Panics if the destination CPU reports ready but its slot area or local IPI
/// queue is not actually initialized.
pub fn run_on_cpu<T: Into<Callback>>(dest_cpu: LogicalCpuId, callback: T) -> Result<()> {
    let cpu_num = kbuild_config::NR_CPUS;
    let dest_cpu_index = dest_cpu.as_usize();

    // Error handling: check CPU ID validity
    if dest_cpu_index >= cpu_num {
        error!("Invalid CPU ID: {} (max: {})", dest_cpu_index, cpu_num - 1);
        return Err(KipiError::InvalidCpuId);
    }

    if raw_cpu_id(dest_cpu).is_none() {
        error!(
            "CPU {} is not present in the logical CPU map",
            dest_cpu_index
        );
        return Err(KipiError::InvalidCpuId);
    }

    debug!("Send IPI event to CPU {}", dest_cpu_index);

    if dest_cpu == this_cpu_id() {
        // Execute callback on current CPU immediately
        callback.into().call();
    } else {
        if !is_ipi_queue_ready(dest_cpu) {
            error!(
                "CPU {} has not initialized its IPI queue yet",
                dest_cpu_index
            );
            return Err(KipiError::TargetCpuNotReady);
        }
        #[expect(
            clippy::expect_used,
            reason = "CG-021-no-panic-on-reachable-failure: IPI readiness is published only after \
                      CPU-area and queue initialization; validated online CPUs retain both"
        )]
        ipi_queue(dest_cpu)
            .expect("ready IPI CPU area must be published")
            .get()
            .expect("ready IPI queue must be initialized")
            .lock()
            .push(this_cpu_id(), callback.into());
        kirq::notify_cpu(IPI_IRQ, IpiTarget::Specific(dest_cpu_index));
    }

    Ok(())
}

/// Executes a callback on all other CPUs via IPI.
///
/// The current CPU runs the callback immediately; all other CPUs receive it
/// through their IPI event queues.
///
/// Use this variant when the current CPU may execute the callback in ordinary
/// task context but the other CPUs may execute it in IPI interrupt context.
pub fn run_on_each_cpu<T: Into<MulticastCallback>>(callback: T) -> Result<()> {
    debug!("Send IPI event to all other CPUs");
    let current_cpu_id = this_cpu_id();
    let callback = callback.into();

    validate_broadcast_targets(current_cpu_id, false)?;

    // Execute callback on current CPU immediately
    callback.clone().call();

    // Push the callback to all other CPUs' IPI event queues
    enqueue_broadcast_to_others(current_cpu_id, &callback);

    // Send IPI to all other CPUs to trigger their callbacks
    kirq::notify_cpu(
        IPI_IRQ,
        IpiTarget::AllButSelf {
            me: current_cpu_id.as_usize(),
            total: kcpu_id_map::nr_cpus(),
        },
    );

    Ok(())
}

/// Executes a callback on every CPU from the IPI handler context.
///
/// Unlike [`run_on_each_cpu`], the current CPU also receives the callback
/// through its local IPI queue instead of running it immediately.
///
/// Use this variant when all CPUs, including the caller, must observe the same
/// IPI-handler execution context.
///
/// # Errors
/// Returns `KipiError::InvalidCpuId` when the broadcast target set is invalid
/// for this CPU map.
///
/// # Panics
/// Panics if the current CPU's slot area is not published or its local IPI
/// queue has not been initialized.
pub fn run_on_each_cpu_via_ipi<T: Into<MulticastCallback>>(callback: T) -> Result<()> {
    debug!("Send IPI event to every CPU, including self");
    let current_cpu_id = this_cpu_id();
    let callback = callback.into();

    validate_broadcast_targets(current_cpu_id, true)?;

    // Push to current CPU's queue so it runs from IPI context too
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: IPI readiness is published only after \
                  CPU-area and queue initialization; validated online CPUs retain both"
    )]
    ipi_queue(current_cpu_id)
        .expect("local IPI CPU area must be published")
        .get()
        .expect("local IPI queue must be initialized")
        .lock()
        .push(current_cpu_id, callback.clone().into_unicast());

    enqueue_broadcast_to_others(current_cpu_id, &callback);

    kirq::notify_cpu(IPI_IRQ, IpiTarget::Self_);
    kirq::notify_cpu(
        IPI_IRQ,
        IpiTarget::AllButSelf {
            me: current_cpu_id.as_usize(),
            total: kcpu_id_map::nr_cpus(),
        },
    );

    Ok(())
}

fn validate_broadcast_targets(current_cpu_id: LogicalCpuId, include_self: bool) -> Result<()> {
    if include_self && !is_ipi_queue_ready(current_cpu_id) {
        error!(
            "CPU {} has not initialized its local IPI queue yet",
            current_cpu_id.as_usize()
        );
        return Err(KipiError::TargetCpuNotReady);
    }

    let mut not_ready_cpu = None;
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if not_ready_cpu.is_some() {
            return;
        }
        if cpu_id != current_cpu_id && !is_ipi_queue_ready(cpu_id) {
            not_ready_cpu = Some(cpu_id);
        }
    });
    if let Some(cpu_id) = not_ready_cpu {
        error!(
            "CPU {} has not initialized its IPI queue yet",
            cpu_id.as_usize()
        );
        return Err(KipiError::TargetCpuNotReady);
    }
    Ok(())
}

fn enqueue_broadcast_to_others(current_cpu_id: LogicalCpuId, callback: &MulticastCallback) {
    for_each_present_logical_cpu(|_, cpu_id, _| {
        if cpu_id != current_cpu_id {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: IPI readiness is published only \
                          after CPU-area and queue initialization; validated online CPUs retain \
                          both"
            )]
            ipi_queue(cpu_id)
                .expect("ready IPI CPU area must be published")
                .get()
                .expect("ready IPI queue must be initialized")
                .lock()
                .push(current_cpu_id, callback.clone().into_unicast());
        }
    });
}

/// The handler for IPI events. Retrieves events from the queue and executes callbacks.
///
/// This function is called in interrupt context. If a callback panics or fails,
/// the error is logged but other pending callbacks will still be processed.
///
/// A system-wide stop request is processed before anything else: once a stop
/// is in progress, every non-orchestrator CPU acknowledges and parks here and
/// never reaches the TLB shootdown or callback queues.
///
/// # Panics
/// Panics if the current CPU's slot area is not published or its local IPI
/// queue has not been initialized.
pub fn ipi_handler() {
    stop::handle_stop_request();

    // Process TLB shootdown requests before handling generic callbacks.
    tlb::handle_shootdown();

    notification::handle_pending();

    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: IPI readiness is published only after \
                  CPU-area and queue initialization; validated online CPUs retain both"
    )]
    while let Some((src_cpu_id, callback)) = ipi_queue(this_cpu_id())
        .expect("local IPI CPU area must be published")
        .get()
        .expect("local IPI queue must be initialized")
        .lock()
        .pop_one()
    {
        debug!("Received IPI event from CPU {}", src_cpu_id.as_usize());

        // use logging instead of silent failure
        callback.call();

        // If future needs to track failures, can add error handling inside Callback
    }
}

#[cfg(unittest)]
#[path = "../ut-tests/lib_unittest.rs"]
mod kipi_unittest;
