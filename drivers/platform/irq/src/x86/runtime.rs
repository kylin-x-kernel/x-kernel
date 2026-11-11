// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Post-boot descriptor operations and interrupt delivery for x86.
//!
//! Everything here runs after [`super::init`] published the installed
//! controllers: source classification, mask/unmask, configuration, IPI
//! notification, and the vector-to-domain dispatch path. The boot topology
//! itself is owned by [`super::config`].

use core::sync::atomic::Ordering;

use kcpu_id_map::{KCpuMask, KCpuMaskExt, LogicalCpuId, raw_cpu_id};
use kirq::{
    IrqAffinityApplyError, IrqAffinityDelivery, IrqAffinityPrepare, IrqPolarity, IrqTrigger,
    TargetCpu,
};
use x86_apic::{IoApicInstanceId, IoApicPin};

use super::config::{domain_for_instance, instance_for_domain};

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

/// Resolves the controller instance and local pin named by an IO-APIC
/// descriptor. `hwirq` is never a GSI and the domain, not the pin number,
/// selects the instance.
pub(crate) fn io_pin(desc: kirq::IrqDesc) -> Option<(IoApicInstanceId, IoApicPin)> {
    if desc.controller != kirq::IrqController::IoApic {
        return None;
    }
    let pin = IoApicPin::try_from_usize(desc.hwirq)?;
    let domain = desc.domain?;
    Some((instance_for_domain(domain)?, pin))
}

fn retrigger_io(desc: kirq::IrqDesc) -> bool {
    let accepted =
        io_pin(desc).is_some_and(|(instance, pin)| x86_apic::retrigger_pin(instance, pin));
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

pub(crate) fn is_msi_source(desc: kirq::IrqDesc) -> bool {
    desc.domain == Some(kirq::MSI_DOMAIN)
        && desc.controller == kirq::IrqController::Msi
        && desc.flags.contains(kirq::IrqFlags::MSI)
        && (usize::from(x86_apic::MSIX_VECTOR_BASE)..usize::from(x86_apic::APIC_TIMER_VECTOR))
            .contains(&desc.hwirq)
}

fn mask_io(desc: kirq::IrqDesc) {
    if let Some((instance, pin)) = io_pin(desc) {
        x86_apic::set_pin_enabled(instance, pin, false);
    }
}

fn unmask_io(desc: kirq::IrqDesc) {
    if let Some((instance, pin)) = io_pin(desc) {
        x86_apic::set_pin_enabled(instance, pin, true);
    }
}

/// Encodes the selected logical CPU, physical destination and original mask
/// state into the opaque level token. The token is local to the completion
/// transaction; KIRQ owns the pending request itself.
fn encode_level_token(cpu: LogicalCpuId, destination: u8, was_masked: bool) -> usize {
    cpu.as_usize() | (usize::from(destination) << 8) | (usize::from(was_masked) << 16)
}

fn decode_level_token(token: usize) -> Option<(LogicalCpuId, u8, bool)> {
    let cpu = token & 0xff;
    let destination = u8::try_from((token >> 8) & 0xff).ok()?;
    let was_masked = (token >> 16) & 1 != 0;
    Some((LogicalCpuId::new(cpu), destination, was_masked))
}

fn deferred_target(
    desc: kirq::IrqDesc,
    candidates: &KCpuMask,
) -> Option<(IoApicInstanceId, IoApicPin, LogicalCpuId, u8)> {
    let (instance, pin) = io_pin(desc)?;
    if !x86_apic::retarget_supported(instance, pin) {
        return None;
    }
    candidates.iter_logical().find_map(|cpu| {
        if cpu.as_usize() > usize::from(u8::MAX) {
            return None;
        }
        let apic_id = u8::try_from(raw_cpu_id(cpu)?.as_usize()).ok()?;
        Some((instance, pin, cpu, apic_id))
    })
}

fn deferred_accept(
    desc: kirq::IrqDesc,
    candidates: &KCpuMask,
) -> Result<(), IrqAffinityApplyError> {
    deferred_target(desc, candidates)
        .map(|_| ())
        .ok_or(IrqAffinityApplyError::Rejected)
}

fn deferred_prepare(
    desc: kirq::IrqDesc,
    candidates: &KCpuMask,
    delivery: IrqAffinityDelivery,
) -> Result<IrqAffinityPrepare, IrqAffinityApplyError> {
    let (instance, pin, cpu, destination) =
        deferred_target(desc, candidates).ok_or(IrqAffinityApplyError::Rejected)?;
    let keep_masked = delivery == IrqAffinityDelivery::KeepMasked;
    match x86_apic::pin_trigger_mode(instance, pin) {
        Some(x86_apic::IoApicTriggerMode::Edge) => {
            if x86_apic::retarget_edge(instance, pin, destination, keep_masked) {
                Ok(IrqAffinityPrepare::Complete(KCpuMask::one_shot_logical(
                    cpu,
                )))
            } else {
                Err(IrqAffinityApplyError::HardwareFailure)
            }
        }
        Some(x86_apic::IoApicTriggerMode::Level) => {
            let was_masked = x86_apic::prepare_level_retarget(instance, pin)
                .ok_or(IrqAffinityApplyError::HardwareFailure)?;
            Ok(IrqAffinityPrepare::Pending(encode_level_token(
                cpu,
                destination,
                was_masked,
            )))
        }
        None => Err(IrqAffinityApplyError::Rejected),
    }
}

fn deferred_finish(
    desc: kirq::IrqDesc,
    token: usize,
    delivery: IrqAffinityDelivery,
) -> Result<Option<KCpuMask>, IrqAffinityApplyError> {
    let (instance, pin) = io_pin(desc).ok_or(IrqAffinityApplyError::Rejected)?;
    let (cpu, destination, was_masked) =
        decode_level_token(token).ok_or(IrqAffinityApplyError::Rejected)?;
    let keep_masked = delivery == IrqAffinityDelivery::KeepMasked;
    match x86_apic::finish_level_retarget(instance, pin, destination, was_masked, keep_masked) {
        x86_apic::IoApicRetargetResult::Complete => Ok(Some(KCpuMask::one_shot_logical(cpu))),
        x86_apic::IoApicRetargetResult::Pending => Ok(None),
        x86_apic::IoApicRetargetResult::Failed => Err(IrqAffinityApplyError::HardwareFailure),
    }
}

pub(crate) static IO_DEFERRED: kirq::IrqDeferredAffinityChip = kirq::IrqDeferredAffinityChip {
    accept: deferred_accept,
    prepare: deferred_prepare,
    finish: deferred_finish,
};

pub(crate) fn select_x86_flow(
    desc: kirq::IrqDesc,
    read_trigger: impl FnOnce(IoApicInstanceId, IoApicPin) -> Option<x86_apic::IoApicTriggerMode>,
) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
    use kirq::{IrqController, IrqFlags, IrqFlow, IrqTrigger};
    let unsupported = kirq::IrqDescError::UnsupportedFlow { hwirq: desc.hwirq };
    if desc.domain.is_none()
        && desc.controller == IrqController::Unknown
        && (desc.hwirq == usize::from(x86_apic::APIC_TIMER_VECTOR)
            || desc.hwirq == kbuild_config::IPI_IRQ)
    {
        return Ok(IrqFlow::PerCpu);
    }
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
    let Some((instance, pin)) = io_pin(desc) else {
        return Err(unsupported);
    };
    if desc.affinity != kirq::IrqAffinity::Any
        || !(desc.flags - (IrqFlags::SHARED | IrqFlags::WAKEUP_SOURCE)).is_empty()
    {
        return Err(unsupported);
    }
    let current = read_trigger(instance, pin).ok_or(unsupported)?;
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

pub(crate) fn configure(desc: kirq::IrqDesc) -> Result<(), kirq::IrqDescError> {
    configure_with(desc, io_pin, x86_apic::configure_pin_checked)
}

/// Descriptor-to-backend adapter shared by production and tests.
///
/// Instance resolution and the backend call are injected so a route-generated
/// descriptor can be followed into a recording pin configuration without
/// mapping a controller. `resolve` returning `None` means the descriptor is
/// not owned by an IO-APIC instance and is left to another provider.
pub(crate) fn configure_with(
    desc: kirq::IrqDesc,
    resolve: impl FnOnce(kirq::IrqDesc) -> Option<(IoApicInstanceId, IoApicPin)>,
    configure_pin: impl FnOnce(IoApicInstanceId, IoApicPin, x86_apic::IoApicTriggerPolicy) -> bool,
) -> Result<(), kirq::IrqDescError> {
    let Some((instance, pin)) = resolve(desc) else {
        return Ok(());
    };
    let polarity = match desc.polarity {
        IrqPolarity::High => x86_apic::IoApicPolarity::High,
        IrqPolarity::Low => x86_apic::IoApicPolarity::Low,
        IrqPolarity::Unknown => x86_apic::IoApicPolarity::High,
    };
    let policy = match desc.trigger {
        IrqTrigger::EdgeRising | IrqTrigger::EdgeFalling => {
            x86_apic::IoApicTriggerPolicy::Program {
                trigger: x86_apic::IoApicTriggerMode::Edge,
                polarity,
            }
        }
        IrqTrigger::LevelHigh | IrqTrigger::LevelLow => x86_apic::IoApicTriggerPolicy::Program {
            trigger: x86_apic::IoApicTriggerMode::Level,
            polarity,
        },
        // Unknown trigger means the descriptor does not override the hardware
        // configuration, not that the pin needs no delivery route. Still assign
        // and verify a vector while retaining the programmed trigger bits.
        IrqTrigger::Unknown(_) => x86_apic::IoApicTriggerPolicy::KeepHardware,
    };
    if configure_pin(instance, pin, policy) {
        Ok(())
    } else {
        Err(kirq::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq })
    }
}

pub(crate) fn enable(desc: kirq::IrqDesc, enabled: bool) {
    if desc.hwirq == usize::from(x86_apic::APIC_TIMER_VECTOR) {
        x86_apic::set_local_timer_enabled(enabled);
        return;
    }
    if let Some((instance, pin)) = io_pin(desc) {
        x86_apic::set_pin_enabled(instance, pin, enabled);
    }
}

pub(crate) fn notify_cpu(interrupt_id: usize, target: TargetCpu) {
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

/// # Safety
/// The caller must uphold [`kirq::IntrManagerIf::dispatch_irq`]'s contract.
pub(crate) unsafe fn dispatch_irq(vector: usize) -> Option<kirq::PendingIrq> {
    dispatch_irq_with(
        vector,
        |vector| {
            // SAFETY: the caller supplies the captured vector on the entry CPU.
            unsafe { dispatch_regular_irq(vector) }
        },
        x86_apic::handle_error_interrupt,
    )
}

pub(crate) fn dispatch_irq_with<T>(
    vector: usize,
    dispatch_regular_fn: impl FnOnce(usize) -> Option<T>,
    handle_error_fn: impl FnOnce(),
) -> Option<T> {
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

/// # Safety
/// The caller must uphold [`kirq::IntrManagerIf::dispatch_irq`]'s contract.
unsafe fn dispatch_regular_irq(vector: usize) -> Option<kirq::PendingIrq> {
    let (source, completion_cookie) =
        prepare_regular_delivery(vector, io_apic_delivery, x86_apic::end_of_interrupt)?;
    // SAFETY: classification transfers this vector's unique remaining EOI
    // obligation, or its no-op sentinel, under the caller's entry exclusion.
    Some(unsafe { kirq::PendingIrq::new(source, completion_cookie) })
}

/// Resolves an IO-APIC delivery vector to the route and domain that own it.
///
/// Vectors are assigned dynamically, so the controller instance and pin come
/// from the backend route table rather than from arithmetic on the vector.
fn io_apic_delivery(vector: usize) -> Option<(kirq::IrqDomainId, usize)> {
    let (instance, pin) = x86_apic::io_apic_route(vector)?;
    Some((domain_for_instance(instance)?, pin.as_usize()))
}

// Keep classification and early EOI in one path. The returned cookie transfers
// only the remaining completion obligation, never a second edge/MSI/local EOI.
pub(crate) fn prepare_regular_delivery(
    vector: usize,
    classify_io: impl FnOnce(usize) -> Option<(kirq::IrqDomainId, usize)>,
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
        let Some((domain, pin)) = classify_io(vector) else {
            // The vector is already in service but no live route owns it.
            // Acknowledge it so the local APIC does not keep it pending.
            trace!("IO-APIC IRQ {} has no live route", vector);
            early_eoi();
            return None;
        };
        trace!("IRQ domain {:?} pin {}", domain, pin);
        return Some((kirq::IrqRef::Domain(domain, pin), 1));
    }
    None
}

pub(crate) fn complete_delivery(completion_cookie: usize, eoi: impl FnOnce()) {
    if completion_cookie != 0 {
        eoi();
    }
}
