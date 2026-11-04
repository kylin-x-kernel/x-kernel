// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::{Registers, TimerResidency};
use crate::arch::{VmmArch, aarch64::Aarch64Vhe};

#[cfg(feature = "kernel-tests")]
#[unittest::def_test(serial)]
fn fold_reconciles_physical_active_without_a_task_switch() {
    use super::irq_route;
    use crate::vdev::aarch64::vtimer::VtimerHookFactory;

    if kirq::IntrManagerIf::guest_interface().is_none() {
        return unittest::TestResult::Ignored;
    }
    assert!(Aarch64Vhe::percpu_hw_init());
    assert!(VtimerHookFactory::prepare_route());
    let timer = TimerResidency::new();
    let _irq = kspin::IrqSave::new();
    let previous = Registers::read();
    timer.enter();
    // Seed the state left by a physical acknowledge, then withdraw the level
    // before guest IAR. Keep one IRQ-off interval across setup and observation.
    core::assert!(irq_route::set_local_forwarding(true, false, true));
    core::assert_eq!(
        irq_driver::gic::local_ppi_state_for_test(27),
        Some((false, true, true))
    );
    timer.set_guest_active(false);
    core::assert_eq!(
        irq_driver::gic::local_ppi_state_for_test(27),
        Some((false, false, true))
    );
    timer.set_guest_active(true);
    core::assert_eq!(
        irq_driver::gic::local_ppi_state_for_test(27),
        Some((false, true, true))
    );
    // An EOI may clear virtual activity while an unmasked timer level remains
    // asserted. Reconstruct its physical backing instead of losing reassertion.
    Registers {
        control: 1,
        compare: 0,
        offset_ticks: 0,
    }
    .restore();
    timer.set_guest_active(false);
    core::assert_eq!(
        irq_driver::gic::local_ppi_state_for_test(27),
        Some((false, true, true))
    );
    timer.stop();
    core::assert_eq!(Registers::read().control & 1, 0);
    core::assert_eq!(
        irq_driver::gic::local_ppi_state_for_test(27),
        Some((false, false, false))
    );
    previous.restore();
}

#[unittest::def_test]
fn timer_deadline_uses_unsigned_guest_counter_domain() {
    use super::super::remaining_ticks;
    assert_eq!(remaining_ticks(0, 0, 0, 100), None);
    assert_eq!(remaining_ticks(3, 0, 0, 100), None);
    assert_eq!(remaining_ticks(1, 100, 0, 100), Some(0));
    assert_eq!(remaining_ticks(1, 100, 50, 100), Some(50));
    assert_eq!(remaining_ticks(1, u64::MAX, 0, 100), Some(u64::MAX - 100));
    assert_eq!(remaining_ticks(1, 10, 100, 50), Some(0));
}

#[unittest::def_test(serial)]
fn masked_timer_registers_follow_task_migration() {
    use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

    if !cfg!(feature = "smp") || kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    assert!(Aarch64Vhe::percpu_hw_init());
    let timer = TimerResidency::new();
    let worker = timer.clone();
    let task = ktask::spawn_task(
        ktask::TaskInner::new_pidless_kthread(
            move || {
                {
                    let _irq = kspin::IrqSave::new();
                    worker.enter();
                    Registers {
                        control: 3,
                        compare: 0x12345678,
                        offset_ticks: 0x87654321,
                    }
                    .restore();
                }
                for cpu in [0, 1, 0, 1] {
                    core::assert!(ktask::set_current_affinity(
                        ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(cpu))
                    ));
                    let _irq = kspin::IrqSave::new();
                    core::assert_eq!(khal::percpu::this_cpu_id(), LogicalCpuId::new(cpu));
                    let registers = Registers::read();
                    core::assert_eq!(registers.control & 3, 3);
                    core::assert_eq!(registers.compare, 0x12345678);
                    core::assert_eq!(registers.offset_ticks, 0x87654321);
                }
                worker.stop();
            },
            "vcpu-timer-migration".into(),
            kbuild_config::TASK_STACK_SIZE,
        )
        .with_schedule_observer(timer.clone()),
    );
    let status = task.join();
    assert_eq!(status, 0);
    let state = timer.state.lock();
    assert!(!state.is_started);
    assert!(!state.is_resident);
}
