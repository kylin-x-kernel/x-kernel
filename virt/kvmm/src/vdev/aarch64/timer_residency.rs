// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Task-owned virtual timer register residency. No reference into `Vcpu` is
//! retained by the scheduler; its assembly save area remains exclusively owned.

use alloc::sync::{Arc, Weak};

use aarch64_cpu::registers::{CNTPCT_EL0, CNTVOFF_EL2, Readable, Writeable};
use kcpu_id_map::LogicalCpuId;
use kiface_task_api::{ScheduleOutReason, TaskScheduleObserver};
use kspin::{IrqSave, SpinNoPreempt};

use super::super::irq_route;

#[derive(Clone, Copy, Default)]
struct Registers {
    control: u64,
    compare: u64,
    offset_ticks: u64,
}

impl Registers {
    fn read() -> Self {
        let control;
        let compare;
        // SAFETY: called only by the owning vCPU task at EL2, with IRQs masked.
        unsafe {
            core::arch::asm!(
                "mrs {control}, S3_5_C14_C3_1",
                "mrs {compare}, S3_5_C14_C3_2",
                control = out(reg) control,
                compare = out(reg) compare,
                options(nostack),
            );
        }
        Self {
            control,
            compare,
            offset_ticks: CNTVOFF_EL2.get(),
        }
    }

    fn restore(self) {
        CNTVOFF_EL2.set(self.offset_ticks);
        // SAFETY: the source is masked and this task exclusively owns CNTV on
        // this CPU. Install the compare value before enabling its output.
        unsafe {
            core::arch::asm!(
                "msr S3_5_C14_C3_2, {compare}",
                "msr S3_5_C14_C3_1, {control}",
                "isb",
                compare = in(reg) self.compare,
                control = in(reg) self.control,
                options(nostack),
            );
        }
    }

    fn is_asserted(self) -> bool {
        super::remaining_ticks(
            self.control,
            self.compare,
            self.offset_ticks,
            CNTPCT_EL0.get(),
        ) == Some(0)
    }
}

fn stop_output() {
    // SAFETY: this module is used only at EL2 in the owning task's IRQ-off
    // residency transition. ISB completes disable before the CPU can be reused.
    unsafe { core::arch::asm!("msr S3_5_C14_C3_1, xzr", "isb", options(nostack)) };
}

#[derive(Default)]
struct State {
    registers: Registers,
    is_started: bool,
    is_resident: bool,
    is_irq_enabled: bool,
    is_guest_active: bool,
    has_failed: bool,
    is_stopped: bool,
}

pub(crate) struct TimerResidency {
    // Task-only state. Hardware-mutating methods separately exclude IRQs
    // across the register and forwarding transaction, even for direct callers.
    state: SpinNoPreempt<State>,
    weak: Weak<Self>,
}

unittest::path_mod_test!(tests, "../../../ut-tests/timer_residency.rs");

impl TimerResidency {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new_cyclic(|weak| Self {
            state: SpinNoPreempt::new(State::default()),
            weak: weak.clone(),
        })
    }

    pub(crate) fn enter(&self) {
        let _irq = IrqSave::new();
        let mut state = self.state.lock();
        if state.is_stopped {
            state.has_failed = true;
            return;
        }
        if !state.is_started {
            state.is_started = true;
            state.is_resident = true;
            // A new vCPU starts with its own zero offset and disabled timer,
            // never with the previous resident's register contents.
            stop_output();
            state.registers.restore();
            irq_route::publish_timer_for_current_cpu(self.weak.clone());
            state.has_failed |= !irq_route::set_local_forwarding(true, false, false);
        }
    }

    pub(crate) fn set_irq_enabled(&self, enabled: bool) {
        let _irq = IrqSave::new();
        let mut state = self.state.lock();
        state.is_irq_enabled = enabled;
        if state.is_resident {
            let registers = Registers::read();
            stop_output();
            state.has_failed |= !irq_route::set_local_forwarding(
                true,
                enabled,
                state.is_guest_active || registers.is_asserted(),
            );
            registers.restore();
        }
    }

    pub(crate) fn set_guest_active(&self, is_active: bool) {
        let _irq = IrqSave::new();
        let mut state = self.state.lock();
        state.is_guest_active = is_active;
        if state.is_resident {
            let registers = Registers::read();
            // A guest may lower the level before acknowledging a pending LR.
            // No guest EOI will then deactivate its physical backing. Reconcile
            // even without a task switch, retaining genuinely active delivery.
            stop_output();
            state.has_failed |= !irq_route::set_local_forwarding(
                true,
                state.is_irq_enabled,
                is_active || registers.is_asserted(),
            );
            registers.restore();
        }
    }

    pub(crate) fn is_resident(&self) -> bool {
        let state = self.state.lock();
        state.is_resident
    }

    pub(crate) fn has_failed(&self) -> bool {
        self.state.lock().has_failed
    }

    pub(crate) fn stop(&self) {
        let _irq = IrqSave::new();
        let mut state = self.state.lock();
        if state.is_resident {
            stop_output();
            state.has_failed |= !irq_route::set_local_forwarding(false, false, false);
            irq_route::clear_owner_for_current_task();
        }
        state.is_started = false;
        state.is_resident = false;
        state.is_stopped = true;
    }
}

// SAFETY: transitions touch only the owning CPU's timer under IRQ exclusion,
// plus bounded IRQ/owner state. There is no allocation, wait, task wake or
// run-queue access. The observer owns its storage independently of Vcpu.
unsafe impl TaskScheduleObserver for TimerResidency {
    unsafe fn schedule_out(&self, cpu: LogicalCpuId, reason: ScheduleOutReason) {
        debug_assert!(!karch::local_irq_enabled());
        debug_assert_eq!(cpu, khal::percpu::this_cpu_id());
        let mut state = self.state.lock();
        if !state.is_resident {
            return;
        }
        state.registers = Registers::read();
        stop_output();
        state.has_failed |= !irq_route::set_local_forwarding(false, false, false);
        irq_route::clear_owner_for_current_task();
        state.is_resident = false;
        if reason == ScheduleOutReason::Exited {
            state.is_started = false;
            state.is_stopped = true;
        }
    }

    unsafe fn schedule_in(&self, cpu: LogicalCpuId) {
        debug_assert!(!karch::local_irq_enabled());
        debug_assert_eq!(cpu, khal::percpu::this_cpu_id());
        let mut state = self.state.lock();
        if !state.is_started {
            return;
        }
        stop_output();
        irq_route::publish_timer_for_current_cpu(self.weak.clone());
        state.has_failed |= !irq_route::set_local_forwarding(
            true,
            state.is_irq_enabled,
            state.is_guest_active || state.registers.is_asserted(),
        );
        state.registers.restore();
        state.is_resident = true;
    }
}
