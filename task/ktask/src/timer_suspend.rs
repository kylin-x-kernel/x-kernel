// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Explicit per-CPU clock-event suspension, independent of timekeeping.
//!
//! PM must park ordinary execution and quiesce timer producers, clock repair
//! and IRQ dispatch before entry. Run each operation on its owning CPU with
//! IRQs masked. This module does not rendezvous CPUs, freeze MONOTONIC, settle
//! sleep time or restore interrupt controllers. It preserves software queues.

use core::marker::PhantomData;

use kcpu_id_map::LogicalCpuId;
use kcpu_slot::cpu_slot_cell;
use kspin::NoPreemptIrqSave;

/// Why a local clock-event transition was rejected, without hardware changes.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TimerSuspendError {
    /// Entry requires masked IRQs outside interrupt/NMI and timer wake batches.
    InvalidContext,
    /// This CPU already has an outstanding suspension.
    AlreadySuspended,
    /// The token belongs to a different CPU.
    WrongCpu,
    /// The token has finished or no longer identifies the active suspension.
    StaleToken,
    /// Hardware has already been restored for this suspension.
    AlreadyRestored,
    /// Restore hardware before allowing deadline programming again.
    HardwareNotRestored,
    /// Non-reusable identities have been exhausted on this CPU.
    IdentityExhausted,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Phase {
    Running,
    Suspended,
    Restored,
}

#[derive(Clone, Copy, Debug)]
struct EventState {
    generation: u64,
    phase: Phase,
}

// SAFETY: every EventState is a Copy u64 plus a fieldless enum. Bitwise copies
// retain valid values and own no allocations, references or self-relative
// addresses; no destructor or shared identity is attached to the boot template.
unsafe impl kcpu_slot::StaticSlotValue for EventState {}

impl EventState {
    const INITIAL: Self = Self {
        generation: 0,
        phase: Phase::Running,
    };

    fn begin(&mut self) -> Result<u64, TimerSuspendError> {
        if self.phase != Phase::Running {
            return Err(TimerSuspendError::AlreadySuspended);
        }
        let generation = self
            .generation
            .checked_add(1)
            .ok_or(TimerSuspendError::IdentityExhausted)?;
        self.generation = generation;
        self.phase = Phase::Suspended;
        Ok(generation)
    }

    fn validate(self, generation: u64) -> Result<(), TimerSuspendError> {
        if self.generation != generation || self.phase == Phase::Running {
            return Err(TimerSuspendError::StaleToken);
        }
        Ok(())
    }

    fn restore(&mut self, generation: u64) -> Result<(), TimerSuspendError> {
        self.validate(generation)?;
        if self.phase == Phase::Restored {
            return Err(TimerSuspendError::AlreadyRestored);
        }
        self.phase = Phase::Restored;
        Ok(())
    }

    fn finish(&mut self, generation: u64) -> Result<(), TimerSuspendError> {
        self.validate(generation)?;
        if self.phase != Phase::Restored {
            return Err(TimerSuspendError::HardwareNotRestored);
        }
        self.phase = Phase::Running;
        Ok(())
    }
}

cpu_slot_cell! {
    static LOCAL_EVENT_STATE: crate::timer_suspend::EventState = crate::timer_suspend::EventState::INITIAL;
}

pub(crate) fn is_suspended(guard: &NoPreemptIrqSave) -> bool {
    LOCAL_EVENT_STATE.read(guard).phase != Phase::Running
}

/// Authority to restore and resume one CPU's local timer event.
///
/// The token cannot cross threads and each operation checks the CPU identity;
/// it does not pin a task between calls. PM must retain CPU ownership and exclude
/// ordinary scheduling/migration throughout the transition. Drop deliberately
/// leaves events suspended: automatic rearm could access lost hardware or use
/// an unrestored time base. Never forget this token. On abort restore hardware
/// and timekeeping as needed, then use the same explicit two-step completion.
#[derive(Debug)]
#[must_use = "restore hardware and explicitly resume events, including on abort"]
pub struct LocalTimerSuspend {
    cpu: LogicalCpuId,
    generation: u64,
    _not_send_sync: PhantomData<*mut ()>,
}

fn check_context(guard: &NoPreemptIrqSave) -> Result<(), TimerSuspendError> {
    if kirq::context::is_in_interrupt_context() || crate::api::is_local_timer_rearm_deferred(guard)
    {
        return Err(TimerSuspendError::InvalidContext);
    }
    Ok(())
}

/// Stops the current CPU's clock event and inhibits ordinary reprogramming.
///
/// Requires initialized scheduler, prepared timer slots and event backend,
/// masked IRQs, no interrupt-like context and quiesced competing producers and
/// backend repair. Call on every participating CPU before freezing the clock.
/// Does not allocate, retain a lock, alter software deadlines or invoke callbacks.
/// A late timer IRQ is suppressed while suspended. The CPU must not enter normal
/// idle/repair or task execution until PM explicitly resumes the event.
///
/// # Errors
/// Rejects enabled IRQs, interrupt/batch entry, duplicate suspension or identity
/// exhaustion before disarming hardware.
pub fn suspend_local_timer() -> Result<LocalTimerSuspend, TimerSuspendError> {
    if karch::local_irq_enabled() {
        return Err(TimerSuspendError::InvalidContext);
    }
    let guard = NoPreemptIrqSave::new();
    check_context(&guard)?;
    let mut state = LOCAL_EVENT_STATE.read(&guard);
    let generation = state.begin()?;
    LOCAL_EVENT_STATE.write(&guard, state);
    khal::time::disarm_timer();
    Ok(LocalTimerSuspend {
        cpu: khal::percpu::this_cpu_id(),
        generation,
        _not_send_sync: PhantomData,
    })
}

impl LocalTimerSuspend {
    fn state(&self, guard: &NoPreemptIrqSave) -> Result<EventState, TimerSuspendError> {
        check_context(guard)?;
        if khal::percpu::this_cpu_id() != self.cpu {
            return Err(TimerSuspendError::WrongCpu);
        }
        let state = LOCAL_EVENT_STATE.read(guard);
        state.validate(self.generation)?;
        Ok(state)
    }

    /// Restores one-shot event configuration while keeping programming inhibited.
    ///
    /// PM must first restore interrupt-controller access, vector/routing and
    /// device mappings. Runs on the owning CPU with IRQs masked; no calibration,
    /// clock-source reset, deadline arbitration or callbacks occur. This step
    /// may precede sleep accounting; the countdown remains stopped afterwards.
    ///
    /// # Errors
    /// Rejects invalid context, wrong CPU, stale tokens or duplicate restoration
    /// before touching event hardware.
    pub fn restore_hardware(&mut self) -> Result<(), TimerSuspendError> {
        if karch::local_irq_enabled() {
            return Err(TimerSuspendError::InvalidContext);
        }
        let guard = NoPreemptIrqSave::new();
        let mut state = self.state(&guard)?;
        state.restore(self.generation)?;
        khal::time::restore_local_timer();
        LOCAL_EVENT_STATE.write(&guard, state);
        Ok(())
    }

    /// Releases inhibition and re-arbitrates current local software deadlines.
    ///
    /// PM must restore continuous MONOTONIC and finish sleep compensation and
    /// clock-change processing first. Requires the owning CPU, masked IRQs,
    /// restored hardware, no callback-needed locks and quiesced ordinary
    /// execution. Also completes an aborted suspend after its required rollback.
    /// Reads the current scheduler/prepared deadlines under existing arbitration;
    /// never restores a cached hardware compare or invokes expiry inline. A due
    /// deadline requests a hardware IRQ; delivery occurs after PM permits IRQs.
    /// No remaining deadline leaves the countdown stopped.
    ///
    /// # Errors
    /// Rejects invalid context, wrong CPU, stale tokens or missing hardware
    /// restoration without releasing inhibition or programming a deadline.
    pub fn resume(&mut self) -> Result<(), TimerSuspendError> {
        if karch::local_irq_enabled() {
            return Err(TimerSuspendError::InvalidContext);
        }
        let guard = NoPreemptIrqSave::new();
        let mut state = self.state(&guard)?;
        state.finish(self.generation)?;
        LOCAL_EVENT_STATE.write(&guard, state);
        crate::api::rearm_local_timer();
        Ok(())
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/timer_suspend.rs");
