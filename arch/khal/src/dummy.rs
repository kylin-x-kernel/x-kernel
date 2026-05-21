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

struct DummyInit;
struct DummyConsole;
struct DummyRtc;
struct DummyTime;
struct DummyPower;
struct DummyIrq;

#[impl_dev_interface]
impl BootHandler for DummyInit {
    fn prepare_boot_memory(_boot_info: &kplat::boot::BootInfo) {}

    fn firmware_init(_boot_info: &kplat::boot::BootInfo) {}

    fn early_driver_init() {}

    fn final_init(_boot_info: &kplat::boot::BootInfo) {}

    #[cfg(feature = "smp")]
    fn final_init_ap(_logical_cpu_id: LogicalCpuId) {}
}

#[impl_dev_interface]
impl crate::console::ConsoleIf for DummyConsole {
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

#[crate_interface::impl_interface]
impl crate::rtc::RtcIf for DummyRtc {
    fn offset_ns() -> u64 {
        0
    }
}

#[impl_dev_interface]
impl crate::time::MonotonicTimerIf for DummyTime {
    fn now_ticks() -> u64 {
        0
    }

    fn t2ns(ticks: u64) -> u64 {
        ticks
    }

    fn ns2t(nanos: u64) -> u64 {
        nanos
    }

    fn freq() -> u64 {
        0
    }

    fn interrupt_id() -> usize {
        0
    }

    fn arm_timer(_deadline_ns: u64) {}
}

#[impl_dev_interface]
impl SysCtrl for DummyPower {
    #[cfg(feature = "smp")]
    fn boot_ap(_logical_cpu_id: LogicalCpuId, _stack_top_paddr: usize) {}

    fn shutdown() -> ! {
        unimplemented!()
    }
}

#[impl_dev_interface]
impl crate::irq::IntrManagerIf for DummyIrq {
    fn configure(_desc: crate::irq::IrqDesc) {}

    fn enable(_irq: usize, _enabled: bool) {}

    fn dispatch_irq(_irq: usize) -> Option<crate::irq::DispatchedIrq> {
        None
    }

    fn complete_irq(_completion_cookie: usize) {}

    fn notify_cpu(_irq: usize, _target: TargetCpu) {}

    fn set_prio(_irq: usize, _priority: u8) {}
}
