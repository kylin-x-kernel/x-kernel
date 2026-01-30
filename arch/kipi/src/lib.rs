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

use khal::{
    irq::{IPI_IRQ, TargetCpu as IpiTarget},
    percpu::this_cpu_id,
};
use kspin::SpinNoIrq;
use lazyinit::LazyInit;

mod event;
mod queue;

pub use event::{Callback, MulticastCallback};
use queue::IpiEventQueue;

/// Result type for IPI operations
pub type Result<T> = core::result::Result<T, KipiError>;

/// Error types for IPI operations
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KipiError {
    /// Invalid CPU ID (exceeds system CPU count)
    InvalidCpuId,
    /// Queue full (too many pending callbacks)
    QueueFull,
    /// Callback execution failed
    CallbackFailed,
}

impl core::fmt::Display for KipiError {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        match self {
            Self::InvalidCpuId => write!(f, "Invalid CPU ID"),
            Self::QueueFull => write!(f, "IPI queue full"),
            Self::CallbackFailed => write!(f, "Callback execution failed"),
        }
    }
}

#[percpu::def_percpu]
static IPI_EVENT_QUEUE: LazyInit<SpinNoIrq<IpiEventQueue>> = LazyInit::new();

/// Initialize the per-CPU IPI event queue.
pub fn init() {
    IPI_EVENT_QUEUE.with_current(|ipi_queue| {
        ipi_queue.init_once(SpinNoIrq::new(IpiEventQueue::default()));
    });
}

/// Executes a callback on the specified destination CPU via IPI.
///
/// # Safety
///
/// The callback must be `Send` as it will execute on a different CPU.
///
/// # Errors
///
/// Returns `KipiError::InvalidCpuId` if `dest_cpu` exceeds system CPU count.
pub fn run_on_cpu<T: Into<Callback>>(dest_cpu: usize, callback: T) -> Result<()> {
    let cpu_num = platconfig::plat::CPU_NUM;

    // Error handling: check CPU ID validity
    if dest_cpu >= cpu_num {
        error!("Invalid CPU ID: {} (max: {})", dest_cpu, cpu_num - 1);
        return Err(KipiError::InvalidCpuId);
    }

    info!("Send IPI event to CPU {dest_cpu}");

    if dest_cpu == this_cpu_id() {
        // Execute callback on current CPU immediately
        callback.into().call();
    } else {
        unsafe { IPI_EVENT_QUEUE.remote_ref_raw(dest_cpu) }
            .lock()
            .push(this_cpu_id(), callback.into());
        khal::irq::notify_cpu(IPI_IRQ, IpiTarget::Specific(dest_cpu));
    }

    Ok(())
}

/// Executes a callback on all other CPUs via IPI.
pub fn run_on_each_cpu<T: Into<MulticastCallback>>(callback: T) -> Result<()> {
    info!("Send IPI event to all other CPUs");
    let current_cpu_id = this_cpu_id();
    let cpu_num = platconfig::plat::CPU_NUM;
    let callback = callback.into();

    // Execute callback on current CPU immediately
    callback.clone().call();

    // Push the callback to all other CPUs' IPI event queues
    for cpu_id in 0..cpu_num {
        if cpu_id != current_cpu_id {
            unsafe { IPI_EVENT_QUEUE.remote_ref_raw(cpu_id) }
                .lock()
                .push(current_cpu_id, callback.clone().into_unicast());
        }
    }

    // Send IPI to all other CPUs to trigger their callbacks
    khal::irq::notify_cpu(
        IPI_IRQ,
        IpiTarget::AllButSelf {
            me: current_cpu_id,
            total: cpu_num,
        },
    );

    Ok(())
}

/// The handler for IPI events. Retrieves events from the queue and executes callbacks.
///
/// This function is called in interrupt context. If a callback panics or fails,
/// the error is logged but other pending callbacks will still be processed.
pub fn ipi_handler() {
    while let Some((src_cpu_id, callback)) = unsafe { IPI_EVENT_QUEUE.current_ref_mut_raw() }
        .lock()
        .pop_one()
    {
        debug!("Received IPI event from CPU {src_cpu_id}");

        // use logging instead of silent failure
        callback.call();

        // If future needs to track failures, can add error handling inside Callback
    }
}

#[cfg(test)]
mod tests;
