// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dummy implementation of platform-related interfaces defined in [`kplat`].

use crate::irq::TargetCpu;
use kcpu_id_map::LogicalCpuId;
use kplat::{
    boot::BootHandler,
    impl_dev_interface,
    sys::SysCtrl,
};

#[impl_dev_interface]
impl BootHandler {
    fn prepare_boot_memory(_boot_info: &kplat::boot::BootInfo) {}

    fn firmware_init(_boot_info: &kplat::boot::BootInfo) {}

    fn early_driver_init() {}

    fn final_init(_boot_info: &kplat::boot::BootInfo) {}

    #[cfg(feature = "smp")]
    fn final_init_ap(_logical_cpu_id: LogicalCpuId) {}
}

#[impl_dev_interface]
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

    fn interrupt_id() -> Option<usize> {
        None
    }
}

#[impl_dev_interface]
impl crate::time::MonotonicTimerIf {
    fn now_ticks() -> crate::time::TimerTicks {
        crate::time::TimerTicks::from_raw(0)
    }

    fn ticks_to_span(ticks: crate::time::TimerTicks) -> ktime_types::TimeSpan {
        ktime_types::TimeSpan::from_nanos(ticks.as_raw())
    }

    fn span_to_ticks(span: ktime_types::TimeSpan) -> crate::time::TimerTicks {
        crate::time::TimerTicks::from_raw(span.as_nanos_u64_saturating())
    }

    fn freq() -> u64 {
        0
    }

    fn interrupt_id() -> usize {
        0
    }

    fn arm_timer(_deadline: crate::time::MonotonicInstant) {}

    fn handle_idle_return(_previous_ticks: crate::time::TimerTicks) -> bool {
        false
    }
}

#[impl_dev_interface]
impl SysCtrl {
    #[cfg(feature = "smp")]
    fn boot_ap(_logical_cpu_id: LogicalCpuId, _stack_top_paddr: usize) -> kerrno::KResult {
        Ok(())
    }

    fn shutdown() -> ! {
        unimplemented!()
    }
}

#[impl_dev_interface]
impl crate::irq::IntrManagerIf {
    fn configure(_desc: crate::irq::IrqDesc) {}

    fn enable(_irq: usize, _enabled: bool) {}

    fn dispatch_irq(_irq: usize) -> Option<crate::irq::DispatchedIrq> {
        None
    }

    fn dispatch_nmi(_irq: usize) -> Option<crate::irq::DispatchedIrq> {
        None
    }

    fn complete_irq(_completion_cookie: usize) {}

    fn notify_cpu(_irq: usize, _target: TargetCpu) {}

    fn set_prio(_irq: usize, _priority: u8) {}
}
