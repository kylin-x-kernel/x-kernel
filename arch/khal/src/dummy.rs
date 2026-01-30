// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Dummy implementation of platform-related interfaces defined in [`kplat`].

use kplat::{
    boot::BootHandler,
    impl_dev_interface,
    interrupts::{Handler, IntrManager, TargetCpu},
    io::ConsoleIf,
    memory::{HwMemory, MemRange},
    sys::SysCtrl,
    timer::GlobalTimer,
};

struct DummyInit;
struct DummyConsole;
struct DummyMem;
struct DummyTime;
struct DummyPower;
struct DummyIrq;

#[impl_dev_interface]
impl BootHandler for DummyInit {
    fn early_init(_cpu_id: usize, _arg: usize) {}

    #[cfg(feature = "smp")]
    fn early_init_ap(_cpu_id: usize) {}

    fn final_init(_cpu_id: usize, _arg: usize) {}

    #[cfg(feature = "smp")]
    fn final_init_ap(_cpu_id: usize) {}
}

#[impl_dev_interface]
impl ConsoleIf for DummyConsole {
    fn write_data(_bytes: &[u8]) {
        unimplemented!()
    }

    fn read_data(_bytes: &mut [u8]) -> usize {
        unimplemented!()
    }

    fn interrupt_id() -> Option<usize> {
        None
    }
}

#[impl_dev_interface]
impl HwMemory for DummyMem {
    fn ram_regions() -> &'static [MemRange] {
        &[]
    }

    fn rsvd_regions() -> &'static [MemRange] {
        &[]
    }

    fn mmio_regions() -> &'static [MemRange] {
        &[]
    }

    fn p2v(_paddr: memaddr::PhysAddr) -> memaddr::VirtAddr {
        va!(0)
    }

    fn v2p(_vaddr: memaddr::VirtAddr) -> memaddr::PhysAddr {
        pa!(0)
    }

    fn kernel_layout() -> (memaddr::VirtAddr, usize) {
        (va!(0), 0)
    }
}

#[impl_dev_interface]
impl GlobalTimer for DummyTime {
    fn now_ticks() -> u64 {
        0
    }

    fn t2ns(ticks: u64) -> u64 {
        ticks
    }

    fn ns2t(nanos: u64) -> u64 {
        nanos
    }

    fn offset_ns() -> u64 {
        0
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
    fn boot_ap(_cpu_id: usize, _stack_top_paddr: usize) {}

    fn shutdown() -> ! {
        unimplemented!()
    }
}

#[impl_dev_interface]
impl IntrManager for DummyIrq {
    fn enable(_irq: usize, _enabled: bool) {}

    fn reg_handler(_irq: usize, _handler: Handler) -> bool {
        false
    }

    fn unreg_handler(_irq: usize) -> Option<Handler> {
        None
    }

    fn dispatch_irq(_irq: usize) -> Option<usize> {
        None
    }

    fn notify_cpu(_irq: usize, _target: TargetCpu) {}

    fn set_prio(_irq: usize, _priority: u8) {}

    fn save_disable() -> usize {
        0
    }

    fn restore(_flag: usize) {}

    fn enable_local() {}

    fn disable_local() {}

    fn is_enabled() -> bool {
        false
    }
}
