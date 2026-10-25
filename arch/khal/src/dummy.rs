// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dummy implementation of platform-related interfaces defined in [`kplat`].

use kcpu_id_map::LogicalCpuId;
use kplat::{boot::BootHandler, sys::SysCtrl};

#[kiface::provide]
impl BootHandler {
    fn prepare_boot_memory(_boot_info: &kplat::boot::BootInfo) {}

    fn firmware_init(_boot_info: &kplat::boot::BootInfo) {}

    fn early_driver_init() {}

    fn prepare_interrupts(_boot_info: &kplat::boot::BootInfo) -> kerrno::KResult<()> {
        Ok(())
    }

    #[cfg(feature = "smp")]
    fn prepare_local_interrupts(_logical_cpu_id: LogicalCpuId) -> kerrno::KResult<()> {
        Ok(())
    }

    fn activate_local_interrupts() -> kerrno::KResult<()> {
        Ok(())
    }
}

#[kiface::provide]
impl crate::console::ConsoleIf {
    fn write_data(_bytes: &[u8]) {
        unimplemented!()
    }

    fn read_data(_bytes: &mut [u8]) -> usize {
        unimplemented!()
    }

    fn write_data_atomic(_bytes: &[u8]) {
        unimplemented!()
    }

    fn prepare_input() {}

    fn interrupt_id() -> Option<usize> {
        None
    }
}

#[kiface::provide]
impl crate::time::ClockSourceIf {
    fn now_ticks() -> crate::time::TimerTicks {
        crate::time::TimerTicks::from_raw(0)
    }

    fn ticks_to_span(ticks: crate::time::TimerTicks) -> ktime_types::TimeSpan {
        ktime_types::TimeSpan::from_nanos(ticks.as_raw())
    }

    fn span_to_ticks(span: ktime_types::TimeSpan) -> crate::time::TimerTicks {
        crate::time::TimerTicks::from_raw(span.as_nanos_u64_saturating())
    }

    fn frequency() -> ktime_types::Frequency {
        ktime_types::Frequency::ZERO
    }
}

#[kiface::provide]
impl crate::time::ClockEventIf {
    fn interrupt_id() -> usize {
        0
    }

    fn arm_timer(_deadline: crate::time::MonotonicInstant) {}

    fn disarm_timer() {}

    fn handle_idle_return(_previous_ticks: crate::time::TimerTicks) -> bool {
        false
    }
}

#[kiface::provide]
impl SysCtrl {
    #[cfg(feature = "smp")]
    fn boot_ap(_logical_cpu_id: LogicalCpuId, _stack_top_paddr: usize) -> kerrno::KResult {
        Ok(())
    }

    fn halt() -> ! {
        unimplemented!()
    }

    fn power_off() -> ! {
        unimplemented!()
    }

    fn suspend_to_ram() -> kerrno::KResult {
        unimplemented!()
    }
}

#[kiface::provide]
impl kirq::IntrManagerIf {
    fn prepare_local_irq_work() -> Option<fn()> {
        None
    }

    fn configure(_desc: kirq::IrqDesc) {}

    fn enable(_irq: usize, _enabled: bool) {}

    fn dispatch_irq(_irq: usize) -> Option<kirq::Virq> {
        None
    }

    fn dispatch_nmi(_irq: usize) {}

    fn complete_irq(_completion_cookie: usize) {}

    fn notify_cpu(_irq: usize, _target: kirq::TargetCpu) {}

    fn set_prio(_irq: usize, _priority: u8) {}
}
