// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real GICv2 pending delivery; only the external QEMU watchdog bounds failure.
//! Route registration is permanent, as in production. Each probe disables its
//! source and clears delegation before releasing the CPU.

use core::sync::atomic::Ordering;

use super::*;
use crate::{
    arch::{VmmArch, aarch64::Aarch64Vhe},
    vdev::aarch64::vtimer::{TimerResidency, VtimerHookFactory},
};

struct GuestInjections(core::sync::atomic::AtomicUsize);

impl crate::vdev::IrqController for GuestInjections {
    fn inject_irq(&self, vcpu: u32, irq: u32) {
        core::assert_eq!((vcpu, irq), (0, 27));
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn deliver_late_ppi() {
    let count = TEST_DELIVERIES.load_current();
    {
        let _irq = kspin::IrqSave::new();
        // SAFETY: this serial fixture owns the resident's disabled CNTV
        // producer. Its caller pins this CPU and drains delivery below.
        core::assert!(unsafe { irq_driver::gic::pend_local_ppi_for_test(27) });
    }
    while TEST_DELIVERIES.load_current() == count {
        core::hint::spin_loop();
    }
    core::assert_eq!(TEST_DELIVERIES.load_current(), count + 1);
}

#[unittest::def_test(serial)]
fn late_ppi_does_not_resurrect_stopped_owner_after_cpu_reuse() {
    use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};

    if kirq::IntrManagerIf::guest_interface().is_none() {
        return unittest::TestResult::Ignored;
    }
    assert!(VtimerHookFactory::prepare_route());
    let old = TimerResidency::new();
    let old_weak = Arc::downgrade(&old);
    let previous = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            core::assert!(Aarch64Vhe::percpu_hw_init());
            let _pin = kspin::NoPreempt::new();
            old.enter();
            old.stop();
            ktask::current().clear_interrupt();
        },
        "vcpu-previous-owner".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    previous.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0)));
    ktask::activate_task(&previous);
    assert_eq!(previous.join(), 0);
    assert!(old_weak.upgrade().is_none());
    let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
        move || {
            let next = TimerResidency::new();
            let vm = crate::vm::Vm::<Aarch64Vhe>::new(crate::vm::VmConfig::new(0, 0, 1))
                .expect("test VM");
            let injections = Arc::new(GuestInjections(core::sync::atomic::AtomicUsize::new(0)));
            vm.shared().devices().set_irq_controller(injections.clone());
            let mut vcpu = vm.create_vcpu(0);
            let _pin = kspin::NoPreempt::new();
            core::assert!(old_weak.upgrade().is_none());
            core::assert!(!ktask::KtaskRef::ptr_eq(&previous, &ktask::current()));
            ktask::current().clear_interrupt();

            // Reopen delivery in host mode solely to deliver the stale event
            // after teardown. There is deliberately no resident to wake.
            let desc = kirq::gic_level_irq_desc(27);
            core::assert!(kirq::try_enable(desc, true).is_ok());
            deliver_late_ppi();
            core::assert!(!ktask::current().is_interrupted());
            core::assert!(!previous.is_interrupted());
            core::assert!(current_timer().is_none());
            core::assert_eq!(
                irq_driver::gic::local_ppi_state_for_test(27),
                Some((true, false, false))
            );
            core::assert!(kirq::try_enable(desc, false).is_ok());

            next.enter();
            next.set_irq_enabled(true);
            deliver_late_ppi();
            core::assert!(ktask::current().is_interrupted());
            core::assert!(!previous.is_interrupted());
            core::assert!(Arc::ptr_eq(&current_timer().expect("new resident"), &next));
            // A physical wake is not guest injection: the next guest starts
            // with a disabled timer, and its own level must be resampled.
            {
                use crate::vdev::VcpuHook;
                let _irq = kspin::IrqSave::new();
                let mut hook = crate::vdev::aarch64::vtimer::HostVtimerHook;
                hook.on_entry(&mut vcpu);
                core::assert_eq!(injections.0.load(Ordering::Relaxed), 0);
                // Positive control: the same entry hook injects only after the
                // new guest's own timer is enabled and due.
                vcpu.arch.cntv_ctl = 1;
                vcpu.arch.cntv_cval = 0;
                hook.on_entry(&mut vcpu);
                core::assert_eq!(injections.0.load(Ordering::Relaxed), 1);
            }
            next.set_guest_active(false);
            next.stop();
            ktask::current().clear_interrupt();
            core::assert_eq!(
                irq_driver::gic::local_ppi_state_for_test(27),
                Some((false, false, false))
            );
            core::assert!(old_weak.upgrade().is_none());
        },
        "vcpu-late-ppi".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    task.set_cpumask(ktask::KCpuMask::one_shot_logical(LogicalCpuId::new(0)));
    ktask::activate_task(&task);
    assert_eq!(task.join(), 0);
}
