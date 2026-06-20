// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel Inter-Processor Communication (IPC) API
//!
//! This module provides a lightweight abstraction for CPU-to-CPU communication
//! using Inter-Processor Interrupts (IPI). It maintains per-CPU callback queues
//! and dispatches callbacks asynchronously upon IPI interrupt reception.
//!
//! ## Safety
//!
//! All callbacks must be `Send` as they execute on different CPUs.

#![cfg_attr(not(test), no_std)]

#[macro_use]
extern crate log;
extern crate alloc;

use core::sync::atomic::{AtomicBool, Ordering};

use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu, raw_cpu_id};
use khal::{
    irq::{IPI_IRQ, TargetCpu as IpiTarget},
    percpu::this_cpu_id,
};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

mod event;
mod icache;
mod queue;
pub mod tlb;

pub use event::{Callback, MulticastCallback};
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

#[percpu::def_percpu]
static IPI_EVENT_QUEUE: LazyInit<SpinNoIrq<IpiEventQueue>> = LazyInit::new();

static IPI_QUEUE_READY: [AtomicBool; kbuild_config::CPU_NUM] =
    [const { AtomicBool::new(false) }; kbuild_config::CPU_NUM];

#[inline]
fn is_ipi_queue_ready(cpu_id: LogicalCpuId) -> bool {
    IPI_QUEUE_READY[cpu_id.as_usize()].load(Ordering::Acquire)
}

/// Initialize the per-CPU IPI event queue.
pub fn init() {
    let cpu_id = this_cpu_id();
    IPI_EVENT_QUEUE.with_current(|ipi_queue| {
        ipi_queue.init_once(SpinNoIrq::new(IpiEventQueue::default()));
    });
    IPI_QUEUE_READY[cpu_id.as_usize()].store(true, Ordering::Release);
}

/// Executes a callback on the specified destination CPU via IPI.
///
/// # Safety
///
/// The callback must be `Send` as it will execute on a different CPU.
///
/// # Errors
///
/// Returns `KipiError::InvalidCpuId` if `dest_cpu` is outside the configured
/// range or does not belong to a present logical CPU.
///
/// Returns `KipiError::TargetCpuNotReady` if the destination CPU exists but
/// has not finished initializing its local IPI queue.
pub fn run_on_cpu<T: Into<Callback>>(dest_cpu: LogicalCpuId, callback: T) -> Result<()> {
    let cpu_num = kbuild_config::CPU_NUM;
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
        // SAFETY: `dest_cpu` was validated as present in the logical CPU map,
        // and `is_ipi_queue_ready(dest_cpu)` proves the target CPU has already
        // initialized its local per-CPU queue storage.
        unsafe { IPI_EVENT_QUEUE.remote_ref_raw(dest_cpu_index) }
            .lock()
            .push(this_cpu_id(), callback.into());
        khal::irq::notify_cpu(IPI_IRQ, IpiTarget::Specific(dest_cpu_index));
    }

    Ok(())
}

/// Executes a callback on all other CPUs via IPI.
///
/// The current CPU runs the callback immediately; all other CPUs receive it
/// through their IPI event queues.
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
    khal::irq::notify_cpu(
        IPI_IRQ,
        IpiTarget::AllButSelf {
            me: current_cpu_id.as_usize(),
            total: kbuild_config::CPU_NUM,
        },
    );

    Ok(())
}

/// Executes a callback on every CPU from the IPI handler context.
///
/// Unlike [`run_on_each_cpu`], the current CPU also receives the callback
/// through its local IPI queue instead of running it immediately.
pub fn run_on_each_cpu_via_ipi<T: Into<MulticastCallback>>(callback: T) -> Result<()> {
    debug!("Send IPI event to every CPU, including self");
    let current_cpu_id = this_cpu_id();
    let callback = callback.into();

    validate_broadcast_targets(current_cpu_id, true)?;

    // Push to current CPU's queue so it runs from IPI context too
    // SAFETY: this runs on the current CPU, whose local per-CPU queue storage
    // has already been initialized before cross-CPU IPI use is allowed.
    unsafe { IPI_EVENT_QUEUE.current_ref_mut_raw() }
        .lock()
        .push(current_cpu_id, callback.clone().into_unicast());

    enqueue_broadcast_to_others(current_cpu_id, &callback);

    khal::irq::notify_cpu(IPI_IRQ, IpiTarget::Self_);
    khal::irq::notify_cpu(
        IPI_IRQ,
        IpiTarget::AllButSelf {
            me: current_cpu_id.as_usize(),
            total: kbuild_config::CPU_NUM,
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
            // SAFETY: `validate_broadcast_targets` already ensured every
            // destination present CPU has initialized its local per-CPU queue
            // storage before any enqueue happens.
            unsafe { IPI_EVENT_QUEUE.remote_ref_raw(cpu_id.as_usize()) }
                .lock()
                .push(current_cpu_id, callback.clone().into_unicast());
        }
    });
}

/// The handler for IPI events. Retrieves events from the queue and executes callbacks.
///
/// This function is called in interrupt context. If a callback panics or fails,
/// the error is logged but other pending callbacks will still be processed.
pub fn ipi_handler() {
    // Process TLB shootdown requests before handling generic callbacks.
    tlb::handle_shootdown();

    // SAFETY: the handler runs on the current CPU and only accesses that CPU's
    // already-initialized local IPI queue.
    while let Some((src_cpu_id, callback)) = unsafe { IPI_EVENT_QUEUE.current_ref_mut_raw() }
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
mod tests;
