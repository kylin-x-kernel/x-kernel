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

static IO_EDGE: kirq::EdgeChip = kirq::EdgeChip {
    mask: mask_io,
    unmask: unmask_io,
    retrigger: retrigger_io,
};
static IO_LEVEL: kirq::FasteoiChip = kirq::FasteoiChip {
    mask: mask_io,
    unmask: unmask_io,
    resend: kirq::FasteoiResend::Level,
};
static MSI_EDGE: kirq::EdgeChip = kirq::EdgeChip {
    // Device MSI-X masking belongs to the PCI owner, not the IO-APIC. Generic
    // admission suppresses callbacks while disabled; this cannot stop a device.
    mask: |_| {},
    unmask: |_| {},
    retrigger: retrigger_msi,
};

fn retrigger_io(desc: kirq::IrqDesc) -> bool {
    let accepted = is_io_source(desc) && x86_apic::retrigger_io_apic(desc.hwirq);
    if !accepted {
        warn!(
            "IO-APIC retrigger rejected for hwirq={}: invalid or changed route; pending retained",
            desc.hwirq
        );
    }
    accepted
}

fn retrigger_msi(desc: kirq::IrqDesc) -> bool {
    let accepted = is_msi_source(desc) && x86_apic::retrigger_msi(desc.hwirq);
    if !accepted {
        warn!(
            "MSI retrigger rejected for vector={}: no live bound route; pending retained",
            desc.hwirq
        );
    }
    accepted
}

fn is_io_source(desc: kirq::IrqDesc) -> bool {
    desc.domain == Some(IO_APIC_DOMAIN)
        && desc.controller == kirq::IrqController::IoApic
        && desc.hwirq < usize::from(x86_apic::MSIX_VECTOR_BASE) - x86_apic::IO_APIC_VECTOR_BASE
}

fn is_msi_source(desc: kirq::IrqDesc) -> bool {
    desc.domain == Some(kirq::MSI_DOMAIN)
        && desc.controller == kirq::IrqController::Msi
        && desc.flags.contains(kirq::IrqFlags::MSI)
        && (usize::from(x86_apic::MSIX_VECTOR_BASE)..usize::from(x86_apic::APIC_TIMER_VECTOR))
            .contains(&desc.hwirq)
}

fn mask_io(desc: kirq::IrqDesc) {
    if is_io_source(desc) {
        x86_apic::set_irq_enabled(desc.hwirq, false);
    }
}

fn unmask_io(desc: kirq::IrqDesc) {
    if is_io_source(desc) {
        x86_apic::set_irq_enabled(desc.hwirq, true);
    }
}

fn select_x86_flow(
    desc: kirq::IrqDesc,
    read_trigger: impl FnOnce(usize) -> Option<x86_apic::IoApicTriggerMode>,
) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
    use kirq::{IrqController, IrqFlags, IrqFlow, IrqTrigger};
    let unsupported = kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq };
    if desc.domain == Some(kirq::MSI_DOMAIN) || desc.controller == IrqController::Msi {
        if !is_msi_source(desc)
            || desc.trigger != IrqTrigger::EdgeRising
            || desc.polarity == IrqPolarity::Low
            || !(desc.flags - (IrqFlags::MSI | IrqFlags::SHARED)).is_empty()
        {
            return Err(unsupported);
        }
        return Ok(IrqFlow::Edge(&MSI_EDGE));
    }
    if desc.domain != Some(IO_APIC_DOMAIN) && desc.controller != IrqController::IoApic {
        // Local vectors have a distinct per-CPU lifecycle, not shared edge flow.
        return Ok(IrqFlow::Legacy);
    }
    if !is_io_source(desc)
        || desc.affinity != kirq::IrqAffinity::Any
        || !(desc.flags - (IrqFlags::SHARED | IrqFlags::WAKEUP_SOURCE)).is_empty()
    {
        return Err(unsupported);
    }
    let current = read_trigger(desc.hwirq).ok_or(unsupported)?;
    let mode = match desc.trigger {
        IrqTrigger::EdgeRising | IrqTrigger::EdgeFalling => x86_apic::IoApicTriggerMode::Edge,
        IrqTrigger::LevelHigh | IrqTrigger::LevelLow => x86_apic::IoApicTriggerMode::Level,
        IrqTrigger::Unknown(_) => current,
    };
    Ok(match mode {
        x86_apic::IoApicTriggerMode::Edge => IrqFlow::Edge(&IO_EDGE),
        x86_apic::IoApicTriggerMode::Level => IrqFlow::Fasteoi(&IO_LEVEL),
    })
}

fn configure(desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
    if !is_io_source(desc) {
        return Ok(());
    }
    let trigger = match desc.trigger {
        IrqTrigger::EdgeRising | IrqTrigger::EdgeFalling => x86_apic::IoApicTriggerMode::Edge,
        IrqTrigger::LevelHigh | IrqTrigger::LevelLow => x86_apic::IoApicTriggerMode::Level,
        IrqTrigger::Unknown(_) => return Ok(()),
    };
    let polarity = match desc.polarity {
        IrqPolarity::High => x86_apic::IoApicPolarity::High,
        IrqPolarity::Low => x86_apic::IoApicPolarity::Low,
        IrqPolarity::Unknown => x86_apic::IoApicPolarity::High,
    };
    if x86_apic::configure_irq_checked(desc.hwirq, trigger, polarity) {
        Ok(())
    } else {
        Err(kirq::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq })
    }
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
    dispatch_irq_with(
        vector,
        dispatch_regular_irq,
        x86_apic::handle_error_interrupt,
    )
}

fn dispatch_irq_with(
    vector: usize,
    dispatch_regular_fn: impl FnOnce(usize) -> Option<kirq::PendingIrq>,
    handle_error_fn: impl FnOnce(),
) -> Option<kirq::PendingIrq> {
    match vector {
        // The dedicated spurious vector has no ISR entry to acknowledge.
        value if value == usize::from(x86_apic::APIC_SPURIOUS_VECTOR) => None,
        value if value == usize::from(x86_apic::APIC_ERROR_VECTOR) => {
            handle_error_fn();
            None
        }
        _ => dispatch_regular_fn(vector),
    }
}

fn dispatch_regular_irq(vector: usize) -> Option<kirq::PendingIrq> {
    let (source, completion_cookie) = prepare_regular_delivery(
        vector,
        x86_apic::irq_trigger_mode,
        x86_apic::end_of_interrupt,
    )?;
    Some(kirq::PendingIrq::new(source, completion_cookie))
}

// Keep classification and early EOI in one path. The returned cookie transfers
// only the remaining completion obligation, never a second edge/MSI/local EOI.
fn prepare_regular_delivery(
    vector: usize,
    trigger_mode: impl FnOnce(usize) -> Option<x86_apic::IoApicTriggerMode>,
    early_eoi: impl FnOnce(),
) -> Option<(kirq::IrqRef, usize)> {
    if vector >= x86_apic::APIC_TIMER_VECTOR as usize {
        trace!("LAPIC IRQ {}", vector);
        early_eoi();
        return Some((kirq::IrqRef::Virq(vector), 0));
    }
    if vector >= x86_apic::MSIX_VECTOR_BASE as usize {
        trace!("MSI-X IRQ {}", vector);
        return Some((kirq::IrqRef::Domain(kirq::MSI_DOMAIN, vector), 1));
    }
    if vector >= x86_apic::IO_APIC_VECTOR_BASE {
        let hwirq = vector - x86_apic::IO_APIC_VECTOR_BASE;
        if trigger_mode(hwirq) == Some(x86_apic::IoApicTriggerMode::Level) {
            trace!("IRQ {} (level)", hwirq);
            return Some((kirq::IrqRef::Domain(IO_APIC_DOMAIN, hwirq), 1));
        }
        trace!("IRQ {} (edge)", hwirq);
        return Some((kirq::IrqRef::Domain(IO_APIC_DOMAIN, hwirq), 1));
    }
    None
}

fn complete_delivery(completion_cookie: usize, eoi: impl FnOnce()) {
    if completion_cookie != 0 {
        eoi();
    }
}

unittest::path_mod_test!(mod tests, "../ut-tests/x86.rs");

#[kiface::provide]
impl kirq::IntrManagerIf {
    fn select_flow(desc: kirq::IrqDesc) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
        let flow = select_x86_flow(desc, x86_apic::irq_trigger_mode)?;
        if is_msi_source(desc) && !x86_apic::msi_route_matches(desc.hwirq, desc.affinity) {
            return Err(kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq });
        }
        Ok(flow)
    }

    fn prepare_local_irq_work() -> Option<fn()> {
        None
    }

    fn configure(desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
        configure(desc)
    }

    fn enable(desc: kirq::IrqDesc, enabled: bool) {
        enable(desc.hwirq, enabled);
    }

    fn dispatch_irq(vector: usize) -> Option<kirq::Virq> {
        kirq::generic_handle_irq(dispatch_irq(vector)?)
    }

    fn dispatch_nmi(_irq: usize) {}

    fn complete_irq(completion_cookie: usize) {
        complete_delivery(completion_cookie, x86_apic::end_of_interrupt);
    }

    fn notify_cpu(interrupt_id: usize, target: kirq::TargetCpu) {
        notify_cpu(interrupt_id, target);
    }

    fn set_prio(_irq: usize, _priority: u8) {
        debug!("x86 APIC priority programming is not implemented");
    }
}
