// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use khal::irq::TargetCpu;

pub const IO_APIC_DOMAIN: khal::irq::IrqDomainId = khal::irq::IO_APIC_DOMAIN;

pub const fn legacy_irq_desc(hwirq: usize) -> khal::irq::IrqDesc {
    khal::irq::io_apic_irq_desc(hwirq)
}

fn enable(irq: usize, enabled: bool) {
    x86_apic::set_irq_enabled(irq, enabled);
}

fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
    match target {
        TargetCpu::Self_ => x86_apic::send_ipi_self(interrupt_id),
        TargetCpu::Specific(logical_cpu_id) => {
            let logical_cpu_id = LogicalCpuId::new(logical_cpu_id);
            let raw_cpu_id = raw_cpu_id(logical_cpu_id);
            x86_apic::send_ipi_raw(interrupt_id, raw_cpu_id);
        }
        TargetCpu::AllButSelf { me: _, total: _ } => x86_apic::send_ipi_all_but_self(interrupt_id),
    }
}

fn dispatch_irq(vector: usize) -> Option<usize> {
    let irq = if vector >= x86_apic::APIC_TIMER_VECTOR as usize {
        trace!("LAPIC IRQ {}", vector);
        x86_apic::end_of_interrupt();
        return Some(vector);
    } else if vector >= x86_apic::MSIX_VECTOR_BASE as usize {
        let irq = vector;
        trace!("MSI-X IRQ {}", irq);
        x86_apic::end_of_interrupt();
        return Some(irq);
    } else if vector >= x86_apic::IO_APIC_VECTOR_BASE {
        vector - x86_apic::IO_APIC_VECTOR_BASE
    } else {
        return None;
    };

    trace!("IRQ {}", irq);
    x86_apic::end_of_interrupt();
    Some(khal::irq::resolve_hwirq(IO_APIC_DOMAIN, irq))
}

struct X86ApicIrqIfImpl;

#[kplat::impl_dev_interface]
impl khal::irq::IntrManagerIf for X86ApicIrqIfImpl {
    fn configure(_desc: khal::irq::IrqDesc) {}

    fn enable(irq: usize, enabled: bool) {
        enable(irq, enabled);
    }

    fn dispatch_irq(vector: usize) -> Option<usize> {
        dispatch_irq(vector)
    }

    fn notify_cpu(interrupt_id: usize, target: khal::irq::TargetCpu) {
        notify_cpu(interrupt_id, target);
    }

    fn set_prio(_irq: usize, _priority: u8) {
        debug!("x86 APIC priority programming is not implemented");
    }
}
