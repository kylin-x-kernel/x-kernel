// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Opt-in QEMU virt integration, not a hardware-unused-SPI discovery mechanism.
//!
//! KIRQ_TEST_QEMU_VIRT_SPI is a decimal absolute INTID reserved by the test
//! invocation. QEMU v10.0.11 virt has 256 SPIs; INTID240 (SPI208) lies beyond
//! its platform-bus SPI112..175 and other a15irqmap entries. The caller must
//! verify the exact machine configuration has no producer for the chosen SPI.
//! An absent KIRQ mapping or an idle GIC bit alone does not establish ownership.
//! At least two ready CPUs are required unless KIRQ_TEST_QEMU_VIRT_MIN_CPUS=1
//! explicitly requests single-CPU coverage; missing AP readiness is not a pass.
//! No wall-clock deadline is used: the external runner bounds stalled delivery.

extern crate alloc;

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use arm_gic_driver::{
    IntId,
    v3::{Affinity, NmiAttribute, Trigger},
};
use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId};
use unittest::{TestResult, assert, assert_eq, def_test};

use super::GIC;

struct Observation {
    cpu: AtomicUsize,
    deliveries: AtomicUsize,
}

struct ReservedSpi {
    intid: IntId,
    route: Option<Affinity>,
    priority: u8,
    trigger: Trigger,
    virq: Option<kirq::Virq>,
    has_action: bool,
}

impl Drop for ReservedSpi {
    fn drop(&mut self) {
        // Only this task injects the reserved, unwired source. Assertions in
        // the test return normally, so cleanup must also handle partial setup.
        if let Some(virq) = self.virq {
            core::assert!(kirq::try_disable_irq_nosync(virq).is_ok());
        }
        {
            let mut gic = GIC.lock();
            gic.set_irq_enable(self.intid, false);
            core::assert!(gic.wait_for_distributor_rwp().is_ok());
            gic.set_pending(self.intid, false);
        }
        if let Some(virq) = self.virq {
            core::assert_eq!(kirq::try_synchronize_irq(virq), Ok(true));
        }
        // A handler's publication precedes its original EOI. Also allow an
        // already acknowledged, rejected claim to finish; never force ICACTIVER.
        while GIC.lock().is_active(self.intid) {
            core::hint::spin_loop();
        }
        if let Some(virq) = self.virq {
            if self.has_action {
                let owner = kirq::try_free_irq(virq);
                core::assert!(matches!(owner, Ok(Some(_))));
                drop(owner);
            }
            core::assert!(kirq::tests_irq::retire_reserved_test_line(virq));
        }
        let gic = GIC.lock();
        gic.set_target_cpu(self.intid, self.route);
        gic.set_priority(self.intid, self.priority);
        gic.set_cfg(self.intid, self.trigger);
        gic.set_pending(self.intid, false);
        core::assert!(!gic.is_irq_enable(self.intid));
        core::assert!(!gic.is_active(self.intid));
        core::assert!(!gic.is_pending(self.intid));
        core::assert_eq!(gic.get_target_cpu(self.intid), self.route);
        core::assert_eq!(gic.get_priority(self.intid), self.priority);
        core::assert_eq!(gic.get_cfg(self.intid), self.trigger);
    }
}

fn wait_for_delivery(observation: &Observation, expected: usize) {
    // Interrupts remain enabled, including when the test runs on the target.
    // Acquire observes the CPU identity written before the handler's release.
    while observation.deliveries.load(Ordering::Acquire) < expected {
        core::hint::spin_loop();
    }
}

#[def_test(serial)]
fn reserved_qemu_spi_affinity_delivers_to_selected_cpus() -> TestResult {
    let Some(setting) = option_env!("KIRQ_TEST_QEMU_VIRT_SPI") else {
        return TestResult::Ignored;
    };
    let parsed = setting.parse::<usize>();
    assert!(parsed.is_ok());
    let hwirq = parsed.unwrap();
    assert_eq!(
        super::super::ACTIVE_GIC.get(),
        Some(&super::super::GicVersion::V3)
    );
    assert!(GIC.get().is_some());
    let limit = super::super::STANDARD_INTID_LIMIT
        .get()
        .copied()
        .unwrap_or(0);
    assert!((32..limit.min(1020)).contains(&hwirq));
    assert!(karch::local_irq_enabled());
    let ready = kiface_cpu_api::CpuInterruptReadinessIf::ready_cpus();
    let minimum = option_env!("KIRQ_TEST_QEMU_VIRT_MIN_CPUS")
        .unwrap_or("2")
        .parse::<usize>();
    assert!(minimum.is_ok());
    let minimum = minimum.unwrap();
    assert!(minimum >= 1);
    assert!(ready.iter_logical().count() >= minimum);
    let cpu0 = LogicalCpuId::new(0);
    assert!(ready.get_logical(cpu0));
    let domain = super::super::gic_domain();
    if let Some(virq) = kirq::translate_hwirq(domain, hwirq) {
        assert!(kirq::descriptor(virq).is_none());
    }
    // The safe constructor accepts the SPI-relative number, not an INTID.
    let intid = IntId::spi((hwirq - 32) as u32);
    let is_ordinary = {
        let gic = GIC.lock();
        // On non-NMI QEMU, Distributor boot reset establishes Group 1 for
        // these reserved SPIs. On NMI-capable hardware this read also checks
        // accessible Group 1 and affinity routing; never change an attribute.
        !gic.supports_nmi_attributes() || gic.nmi_attribute(intid) == Ok(NmiAttribute::Maskable)
    };
    assert!(is_ordinary);
    let (is_idle, route, priority, trigger) = {
        let gic = GIC.lock();
        (
            !gic.is_irq_enable(intid) && !gic.is_pending(intid) && !gic.is_active(intid),
            gic.get_target_cpu(intid),
            gic.get_priority(intid),
            gic.get_cfg(intid),
        )
    };
    assert!(is_idle);
    let mut fixture = ReservedSpi {
        intid,
        route,
        priority,
        trigger,
        virq: None,
        has_action: false,
    };
    let desc = super::super::edge_irq_desc(hwirq).with_domain(domain);
    let mapped = kirq::try_map(desc);
    assert!(mapped.is_ok());
    let virq = mapped.unwrap();
    fixture.virq = Some(virq);
    let observation = Arc::new(Observation {
        cpu: AtomicUsize::new(usize::MAX),
        deliveries: AtomicUsize::new(0),
    });
    let observed = observation.clone();
    let installed = kirq::try_register_disabled(
        virq,
        Arc::new(move |_| {
            observed
                .cpu
                .store(kplat::cpu::id().as_usize(), Ordering::Relaxed);
            observed.deliveries.fetch_add(1, Ordering::Release);
            kirq::IrqEvent::HANDLED
        }),
    );
    fixture.has_action = matches!(installed, Ok(true));
    assert_eq!(installed, Ok(true));
    // A vacant Distributor entry can initially have priority zero. Never let
    // this ordinary test interrupt inherit an NMI-like priority.
    GIC.lock().set_priority(intid, karch::pmr::IRQ_THRESHOLD);

    let mask0 = KCpuMask::one_shot_logical(cpu0);
    // Public success also proves CPU0 passed runtime's ready-CPU filter.
    let routed = kirq::set_irq_affinity(virq, &mask0);
    assert!(routed.is_ok());
    let route0 = routed.unwrap();
    assert_eq!(route0.requested, Some(mask0));
    assert_eq!(route0.effective, Some(mask0));
    assert!(!route0.has_failed_programming);
    assert_eq!(kirq::irq_affinity(virq), Ok(route0));
    let (is_enabled, is_pending) = {
        let gic = GIC.lock();
        gic.set_pending(intid, true);
        (gic.is_irq_enable(intid), gic.is_pending(intid))
    };
    assert!(!is_enabled);
    assert!(is_pending);
    assert_eq!(observation.deliveries.load(Ordering::Acquire), 0);
    assert!(kirq::try_enable_irq(virq).is_ok());
    wait_for_delivery(&observation, 1);
    assert_eq!(observation.cpu.load(Ordering::Relaxed), 0);
    assert_eq!(kirq::try_synchronize_irq(virq), Ok(true));
    assert_eq!(observation.deliveries.load(Ordering::Acquire), 1);

    for (index, cpu) in ready.iter_logical().filter(|cpu| *cpu != cpu0).enumerate() {
        let expected = index + 2;
        let mask = KCpuMask::one_shot_logical(cpu);
        // The previous claim has drained. This setter must preserve actual
        // enabled state, unlike the disabled first routing transaction.
        let routed = kirq::set_irq_affinity(virq, &mask);
        assert!(routed.is_ok());
        let moved = routed.unwrap();
        assert_eq!(moved.requested, Some(mask));
        assert_eq!(moved.effective, Some(mask));
        assert_eq!(kirq::irq_affinity(virq), Ok(moved));
        let is_enabled = { GIC.lock().is_irq_enable(intid) };
        assert!(is_enabled);
        GIC.lock().set_pending(intid, true);
        wait_for_delivery(&observation, expected);
        assert_eq!(observation.cpu.load(Ordering::Relaxed), cpu.as_usize());
        assert_eq!(kirq::try_synchronize_irq(virq), Ok(true));
        assert_eq!(observation.deliveries.load(Ordering::Acquire), expected);
    }
    drop(fixture);
    assert!(kirq::descriptor(virq).is_none());
    assert_eq!(Arc::strong_count(&observation), 1);
    TestResult::Ok
}
