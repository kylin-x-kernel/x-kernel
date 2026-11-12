// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Post-boot ordinary PMU PPI activation and restoration on each boot CPU.
//! This does not observe the disabled state before startup activation.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, Ordering};

use kerrno::KError;
use unittest::{assert, assert_eq, def_test};

fn on_cpu(cpu: usize, body: impl FnOnce() -> bool + Send + 'static) -> bool {
    let result = Arc::new(AtomicBool::new(false));
    let completed = result.clone();
    let sent = kipi::run_on_cpu_sync(kcpu_id_map::LogicalCpuId::new(cpu), move || {
        completed.store(body(), Ordering::Release);
    });
    sent.unwrap();
    result.load(Ordering::Acquire)
}

#[def_test(serial)]
fn ordinary_pmu_uses_an_enabled_local_action() -> unittest::TestResult {
    #[cfg(feature = "nmi-pmu")]
    if kplat::nm_irq::mode() != kplat::nm_irq::NmiMode::None {
        return unittest::TestResult::Ignored;
    }
    let irq = of::pmu_irq_or(kbuild_config::PMU_IRQ);
    {
        let _pin = kspin::NoPreemptIrqSave::new();
        if irq_driver::gic::local_ppi_state_for_test(irq).is_none() {
            return unittest::TestResult::Ignored;
        }
    }
    let desc = kirq::gic_level_irq_desc(irq);
    // A normal action must exist, not an unprepared NMI owner. This rejection
    // leaves ordinary ownership and admission unchanged.
    assert!(matches!(
        kirq::begin_nmi_shutdown(desc),
        Err(kirq::IrqDescError::NoIrqAction { .. })
    ));
    assert_eq!(khal::pmu::shutdown_overflow_irq(), Err(KError::Unsupported));
    assert_eq!(khal::pmu::restore_overflow_irq(), Err(KError::Unsupported));
    let cpus = if cfg!(feature = "smp") {
        kcpu_id_map::nr_cpus()
    } else {
        1
    };
    for cpu in 0..cpus {
        assert!(on_cpu(cpu, move || {
            let _pin = kspin::NoPreemptIrqSave::new();
            let before = irq_driver::gic::local_ppi_state_for_test(irq);
            let disabled = kirq::try_disable_irq_nosync(desc).is_ok();
            let masked = irq_driver::gic::local_ppi_state_for_test(irq);
            // Restore through the production activation path even on failure.
            khal::pmu::enable_irq();
            let enabled = irq_driver::gic::local_ppi_state_for_test(irq);
            disabled
                && before.is_some_and(|(enabled, ..)| enabled)
                && masked.is_some_and(|(enabled, ..)| !enabled)
                && enabled.is_some_and(|(enabled, ..)| enabled)
                && kirq::try_enable_irq(desc).is_ok()
        }));
    }
    unittest::ktest_println!("PMU ordinary action activated on {} GICv2 CPUs", cpus);
    unittest::TestResult::Ok
}

#[def_test(serial)]
fn ordinary_spi_registration_declares_user_routing_restriction() {
    let options = super::ordinary_spi_register_options();
    assert!(options.is_user_affinity_restricted());
    #[cfg(feature = "nmi-pmu")]
    assert!(!options.should_auto_enable());
    #[cfg(not(feature = "nmi-pmu"))]
    assert!(options.should_auto_enable());
}
