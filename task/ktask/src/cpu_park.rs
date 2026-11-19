// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reversible remote-CPU rendezvous using the scheduler's pinned Stop tasks.
//! This excludes ordinary execution and local IRQ events, not DMA or hardware
//! NMIs. Platform PM must independently quiesce those sources before sleep.

use alloc::{boxed::Box, vec::Vec};
use core::{
    marker::PhantomData,
    sync::atomic::{AtomicBool, AtomicU8, Ordering},
};

use kcpu_id_map::{LogicalCpuId, for_each_present_logical_cpu};
use ktime_types::TimeSpan;

use crate::{CpuStopWork, stop_one_cpu_nowait, timer_suspend::suspend_local_timer};

/// Why remote CPU preparation could not complete.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuParkError {
    /// The coordinator requires sleepable task context before pinning.
    InvalidContext,
    /// A target CPU has no available stop-thread work slot.
    StopperUnavailable,
    /// A CPU could not disarm its local event.
    TimerPause,
    /// Acknowledgments exceeded the preparation budget.
    TimedOut,
}

struct ParkState {
    // Release/acquire publishes successful event suspension (1) or failure (2).
    acknowledged: AtomicU8,
    // The coordinator publishes completion of shared hardware/time recovery.
    release: AtomicBool,
}

struct Participant {
    cpu: LogicalCpuId,
    state: ParkState,
    work: CpuStopWork,
}

fn park_remote(argument: *mut ()) {
    // SAFETY: ParkedCpus keeps this boxed state pinned until the stop work's
    // completion flag confirms the callback has returned and released its slot.
    let state = unsafe { &*argument.cast::<ParkState>() };
    let Ok(mut events) = suspend_local_timer() else {
        state.acknowledged.store(2, Ordering::Release);
        return;
    };
    acknowledge_and_wait(state);
    // A stale token or failed restore here would release execution with an
    // inhibited timer. That is an internal credential violation, not a usable
    // partial recovery state; never silently release ordinary execution.
    assert!(events.restore_hardware().is_ok());
    assert!(events.resume().is_ok());
}

fn acknowledge_and_wait(state: &ParkState) {
    state.acknowledged.store(1, Ordering::Release);
    while !state.release.load(Ordering::Acquire) {
        core::hint::spin_loop();
    }
}

/// Owns acknowledged remote Stop callbacks and pins their coordinator.
///
/// Drop releases CPUs only after the caller restores shared resources, and
/// joins every queued callback before reclaiming storage. Keep ordinary IRQ,
/// NMI and device exclusion separately; this alone is not a firmware-suspend
/// capability. In particular CPUs cannot survive hardware reset through this
/// spin-only rendezvous without an additional platform context-save protocol.
#[must_use = "retain until shared hardware and time are safe for remote execution"]
pub struct ParkedCpus {
    participants: Box<[Participant]>,
    pin: Option<kspin::NoPreempt>,
    _not_send_sync: PhantomData<*mut ()>,
}

impl ParkedCpus {
    /// Parks every other initialized CPU and disarms its local clock event.
    ///
    /// Timeout cancels preparation but still joins queued callbacks before
    /// returning: safely reclaiming their pinned storage requires completion.
    /// A CPU that no longer services Stop work can therefore prevent rollback.
    ///
    /// # Errors
    /// Returns invalid context, unavailable Stop work, event-pause failure or
    /// an acknowledgment timeout after rolling back all queued participants.
    pub fn acquire(timeout: TimeSpan) -> Result<Self, CpuParkError> {
        if !crate::is_sleepable_context() {
            return Err(CpuParkError::InvalidContext);
        }
        let mut participants = Vec::new();
        for_each_present_logical_cpu(|_, cpu, _| {
            participants.push(Participant {
                cpu,
                state: ParkState {
                    acknowledged: AtomicU8::new(0),
                    release: AtomicBool::new(false),
                },
                work: CpuStopWork::empty(),
            });
        });
        let pin = kspin::NoPreempt::new();
        let owner = khal::percpu::this_cpu_id();
        participants.retain(|participant| participant.cpu != owner);
        let mut parked = Self {
            participants: participants.into_boxed_slice(),
            pin: Some(pin),
            _not_send_sync: PhantomData,
        };
        let started = khal::time::monotonic_time();
        for participant in &mut parked.participants {
            let argument = (&participant.state as *const ParkState).cast_mut().cast();
            if !stop_one_cpu_nowait(
                participant.cpu,
                park_remote,
                argument,
                &mut participant.work,
            ) {
                return Err(CpuParkError::StopperUnavailable);
            }
        }
        loop {
            let mut ready = true;
            for participant in &parked.participants {
                match participant.state.acknowledged.load(Ordering::Acquire) {
                    0 => ready = false,
                    1 => {}
                    _ => return Err(CpuParkError::TimerPause),
                }
            }
            if ready {
                return Ok(parked);
            }
            if khal::time::monotonic_time() - started >= timeout {
                return Err(CpuParkError::TimedOut);
            }
            core::hint::spin_loop();
        }
    }

    /// Allows sleepable local recovery after shared time/controller restoration.
    /// # Safety
    /// The owner must have a fixed affinity to its current CPU until this guard
    /// joins all remote callbacks. Local consumers must remain safe while other
    /// CPUs are held; restore hardware before permitting local IRQ execution.
    pub unsafe fn allow_local_scheduling(&mut self) {
        drop(self.pin.take());
    }

    /// Number of remote CPUs with confirmed local event suspension.
    pub fn remote_cpu_count(&self) -> usize {
        self.participants.len()
    }
}

impl Drop for ParkedCpus {
    fn drop(&mut self) {
        for participant in &self.participants {
            participant.state.release.store(true, Ordering::Release);
        }
        for participant in &self.participants {
            while participant.work.is_queued() {
                core::hint::spin_loop();
            }
        }
    }
}

unittest::path_mod_test!(tests, "../ut-tests/cpu_park.rs");
