// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
use core::sync::atomic::Ordering;

use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kirq::{IrqPolarity, IrqTrigger, TargetCpu};

pub const IO_APIC_DOMAIN: kirq::IrqDomainId = kirq::IO_APIC_DOMAIN;

pub const fn legacy_irq_desc(hwirq: usize) -> kirq::IrqDesc {
    kirq::io_apic_irq_desc(hwirq)
}

fn configure(desc: kirq::IrqDesc) {
    let trigger = match desc.trigger {
        IrqTrigger::EdgeRising | IrqTrigger::EdgeFalling => x86_apic::IoApicTriggerMode::Edge,
        IrqTrigger::LevelHigh | IrqTrigger::LevelLow => x86_apic::IoApicTriggerMode::Level,
        IrqTrigger::Unknown(_) => return,
    };
    let polarity = match desc.polarity {
        IrqPolarity::High => x86_apic::IoApicPolarity::High,
        IrqPolarity::Low => x86_apic::IoApicPolarity::Low,
        IrqPolarity::Unknown => x86_apic::IoApicPolarity::High,
    };
    x86_apic::configure_irq(desc.hwirq, trigger, polarity);
}

fn enable(irq: usize, enabled: bool) {
    x86_apic::set_irq_enabled(irq, enabled);
}

fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    // Publish prior Normal-memory stores before programming the local APIC
    // ICR/MSR. x86 TSO orders ordinary WB-memory accesses, but `notify_cpu()`
    // is also the architecture boundary where we transition into device/APIC
    // state. Keep the publish-before-notify guarantee here so callers do not
    // need a separate fence before every IPI send.
    core::sync::atomic::fence(Ordering::SeqCst);
    match target {
        TargetCpu::Self_ => x86_apic::send_ipi_self(interrupt_id),
        TargetCpu::Specific(logical_cpu_id) => {
            let logical_cpu_id = LogicalCpuId::new(logical_cpu_id);
            let Some(raw_cpu_id) = raw_cpu_id(logical_cpu_id) else {
                warn!(
                    "x86 notify_cpu: missing raw CPU id for logical CPU {}",
                    logical_cpu_id.as_usize()
                );
                return;
            };
            x86_apic::send_ipi_raw(interrupt_id, raw_cpu_id);
        }
        TargetCpu::AllButSelf { me: _, total: _ } => x86_apic::send_ipi_all_but_self(interrupt_id),
    }
}

fn dispatch_irq(vector: usize) -> Option<kirq::PendingIrq> {
    if vector >= x86_apic::APIC_TIMER_VECTOR as usize {
        trace!("LAPIC IRQ {}", vector);
        x86_apic::end_of_interrupt();
        return Some(kirq::PendingIrq::new(kirq::IrqRef::Virq(vector), 0));
    }
    if vector >= x86_apic::MSIX_VECTOR_BASE as usize {
        trace!("MSI-X IRQ {}", vector);
        x86_apic::end_of_interrupt();
        return Some(kirq::PendingIrq::new(
            kirq::IrqRef::Domain(kirq::MSI_DOMAIN, vector),
            0,
        ));
    }
    if vector >= x86_apic::IO_APIC_VECTOR_BASE {
        let hwirq = vector - x86_apic::IO_APIC_VECTOR_BASE;
        if x86_apic::irq_trigger_mode(hwirq) == Some(x86_apic::IoApicTriggerMode::Level) {
            trace!("IRQ {} (level)", hwirq);
            return Some(kirq::PendingIrq::new(
                kirq::IrqRef::Domain(IO_APIC_DOMAIN, hwirq),
                1,
            ));
        }
        trace!("IRQ {} (edge)", hwirq);
        x86_apic::end_of_interrupt();
        return Some(kirq::PendingIrq::new(
            kirq::IrqRef::Domain(IO_APIC_DOMAIN, hwirq),
            0,
        ));
    }
    None
}

#[kplat::impl_dev_interface]
impl kirq::IntrManagerIf {
    fn configure(desc: kirq::IrqDesc) {
        configure(desc);
    }

    fn enable(irq: usize, enabled: bool) {
        enable(irq, enabled);
    }

    fn dispatch_irq(vector: usize) -> Option<kirq::Virq> {
        kirq::generic_handle_irq(dispatch_irq(vector)?)
    }

    fn dispatch_nmi(_irq: usize) {}

    fn complete_irq(completion_cookie: usize) {
        if completion_cookie != 0 {
            x86_apic::end_of_interrupt();
        }
    }

    fn notify_cpu(interrupt_id: usize, target: kirq::TargetCpu) {
        notify_cpu(interrupt_id, target);
    }

    fn set_prio(_irq: usize, _priority: u8) {
        debug!("x86 APIC priority programming is not implemented");
    }
}
