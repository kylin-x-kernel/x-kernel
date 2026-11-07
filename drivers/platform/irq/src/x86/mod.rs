// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.
use core::sync::atomic::Ordering;

use fwnode::FwSource;
use kcpu_id_map::{LogicalCpuId, raw_cpu_id};
use kerrno::{KError, KResult};
use kirq::{IrqPolarity, IrqTrigger, TargetCpu};
use memaddr::pa;
use x86_apic::IoApicPin;

pub const IO_APIC_DOMAIN: kirq::IrqDomainId = kirq::IO_APIC_DOMAIN;

pub mod discovery;

pub use x86_apic::IoApicInitPolicy;

/// One IO-APIC controller declaration consumed by the unified entry.
///
/// `id` is the firmware identity and is not used for routing yet. `gsi_base`
/// is the controller's first GSI; the current single-controller runtime only
/// supports base zero.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct IoApicDeclaration {
    pub id: u8,
    pub address: u32,
    pub gsi_base: u32,
}

/// Inputs for the one x86 interrupt-initialization entry.
///
/// This is intentionally limited to what the current runtime consumes; it is
/// not the firmware-discovery container.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct X86IrqInitConfig {
    pub controller: IoApicDeclaration,
    pub policy: IoApicInitPolicy,
    pub source: FwSource,
}

impl X86IrqInitConfig {
    /// Builds a configuration whose declaration comes from firmware discovery.
    pub const fn from_acpi(controller: IoApicDeclaration, policy: IoApicInitPolicy) -> Self {
        Self {
            controller,
            policy,
            source: FwSource::Acpi,
        }
    }

    /// Builds a configuration whose declaration comes from platform policy.
    pub const fn platform_static(controller: IoApicDeclaration, policy: IoApicInitPolicy) -> Self {
        Self {
            controller,
            policy,
            source: FwSource::Static,
        }
    }
}

/// Rejects configurations that cannot be routed before touching hardware.
fn validate_config(config: &X86IrqInitConfig) -> KResult<()> {
    if config.controller.address == 0 {
        return Err(KError::InvalidInput);
    }
    // The runtime has one controller whose hwirq equals the controller pin.
    // A non-zero base would require GSI-to-pin translation that does not
    // exist yet, so reject it instead of programming the wrong pins.
    if config.controller.gsi_base != 0 {
        return Err(KError::Unsupported);
    }
    // Provenance is part of the contract: an unknown source cannot be
    // attributed to platform policy or firmware discovery.
    if config.source == FwSource::Unknown {
        return Err(KError::InvalidInput);
    }
    Ok(())
}

/// Rejects controllers whose pin window this runtime cannot represent.
fn validate_capabilities(
    controller: &IoApicDeclaration,
    capabilities: x86_apic::IoApicCapabilities,
) -> KResult<()> {
    let pin_count = u32::from(capabilities.pin_count);
    let vector_window = x86_apic::MSIX_VECTOR_BASE as u32 - x86_apic::IO_APIC_VECTOR_BASE as u32;
    if pin_count == 0 || pin_count > vector_window {
        return Err(KError::Unsupported);
    }
    if controller.gsi_base.checked_add(pin_count).is_none() {
        return Err(KError::Unsupported);
    }
    Ok(())
}

/// Initializes the boot CPU's Local APIC and IO-APIC.
///
/// Validation of the declaration is pure and happens before any hardware is
/// touched. Hardware-capability validation happens after the IO-APIC is
/// published; on failure the caller must abort boot because hardware
/// preparation is not rolled back.
///
/// # Errors
///
/// [`KError::InvalidInput`] for a zero address or unknown provenance,
/// [`KError::Unsupported`] for a non-zero GSI base, and the backend's errors
/// for mapping or Local APIC owner allocation. A capability rejection also
/// returns [`KError::Unsupported`] after hardware initialization.
///
/// # Panics
///
/// Panics on duplicate Local APIC or IO-APIC publication, as required by the
/// once-only boot contract.
pub fn init(config: X86IrqInitConfig) -> KResult<()> {
    validate_config(&config)?;
    info!(
        "Initialize x86 IRQs: IO-APIC {} at {:#010x}, gsi_base={}, source={:?}",
        config.controller.id, config.controller.address, config.controller.gsi_base, config.source
    );
    x86_apic::init_local_primary()?;
    let capabilities =
        x86_apic::init_io_apic(pa!(config.controller.address as usize), config.policy)?;
    validate_capabilities(&config.controller, capabilities)?;
    Ok(())
}

/// Brings up the Local APIC on an application CPU.
///
/// Runs on each AP after the boot CPU completed [`init`].
///
/// # Errors
///
/// Propagates Local APIC owner-allocation failure (`NoMemory`).
///
/// # Panics
///
/// Panics on missing boot-CPU preparation or duplicate per-CPU initialization.
pub fn init_secondary() -> KResult<()> {
    x86_apic::init_secondary()
}

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
    let accepted = is_io_source(desc)
        && IoApicPin::try_from_usize(desc.hwirq).is_some_and(x86_apic::retrigger_pin);
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

fn io_pin(desc: kirq::IrqDesc) -> Option<IoApicPin> {
    // The current IO-APIC domain uses `hwirq` directly as the controller pin.
    // Firmware GSI-to-pin translation belongs to the discovery slice; this is
    // deliberately an identity conversion until then.
    is_io_source(desc)
        .then(|| IoApicPin::try_from_usize(desc.hwirq))
        .flatten()
}

fn is_msi_source(desc: kirq::IrqDesc) -> bool {
    desc.domain == Some(kirq::MSI_DOMAIN)
        && desc.controller == kirq::IrqController::Msi
        && desc.flags.contains(kirq::IrqFlags::MSI)
        && (usize::from(x86_apic::MSIX_VECTOR_BASE)..usize::from(x86_apic::APIC_TIMER_VECTOR))
            .contains(&desc.hwirq)
}

fn mask_io(desc: kirq::IrqDesc) {
    if let Some(pin) = io_pin(desc) {
        x86_apic::set_pin_enabled(pin, false);
    }
}

fn unmask_io(desc: kirq::IrqDesc) {
    if let Some(pin) = io_pin(desc) {
        x86_apic::set_pin_enabled(pin, true);
    }
}

fn select_x86_flow(
    desc: kirq::IrqDesc,
    read_trigger: impl FnOnce(IoApicPin) -> Option<x86_apic::IoApicTriggerMode>,
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
    if desc.domain != Some(IO_APIC_DOMAIN) && desc.controller != IrqController::IoApic {
        return Err(unsupported);
    }
    if !is_io_source(desc)
        || desc.affinity != kirq::IrqAffinity::Any
        || !(desc.flags - (IrqFlags::SHARED | IrqFlags::WAKEUP_SOURCE)).is_empty()
    {
        return Err(unsupported);
    }
    let pin = IoApicPin::try_from_usize(desc.hwirq).ok_or(unsupported)?;
    let current = read_trigger(pin).ok_or(unsupported)?;
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
    let Some(pin) = IoApicPin::try_from_usize(desc.hwirq) else {
        return Err(kirq::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq });
    };
    if x86_apic::configure_pin_checked(pin, trigger, polarity) {
        Ok(())
    } else {
        Err(kirq::IrqDescError::ConfigurationFailed { hwirq: desc.hwirq })
    }
}

fn enable(irq: usize, enabled: bool) {
    if irq == usize::from(x86_apic::APIC_TIMER_VECTOR) {
        x86_apic::set_local_timer_enabled(enabled);
        return;
    }
    if let Some(pin) = IoApicPin::try_from_usize(irq) {
        x86_apic::set_pin_enabled(pin, enabled);
    }
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

/// # Safety
/// The caller must uphold [`kirq::IntrManagerIf::dispatch_irq`]'s contract.
unsafe fn dispatch_irq(vector: usize) -> Option<kirq::PendingIrq> {
    dispatch_irq_with(
        vector,
        |vector| {
            // SAFETY: the caller supplies the captured vector on the entry CPU.
            unsafe { dispatch_regular_irq(vector) }
        },
        x86_apic::handle_error_interrupt,
    )
}

fn dispatch_irq_with<T>(
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
    let (source, completion_cookie) = prepare_regular_delivery(
        vector,
        x86_apic::pin_trigger_mode,
        x86_apic::end_of_interrupt,
    )?;
    // SAFETY: classification transfers this vector's unique remaining EOI
    // obligation, or its no-op sentinel, under the caller's entry exclusion.
    Some(unsafe { kirq::PendingIrq::new(source, completion_cookie) })
}

// Keep classification and early EOI in one path. The returned cookie transfers
// only the remaining completion obligation, never a second edge/MSI/local EOI.
fn prepare_regular_delivery(
    vector: usize,
    trigger_mode: impl FnOnce(IoApicPin) -> Option<x86_apic::IoApicTriggerMode>,
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
        let pin = IoApicPin::try_from_usize(hwirq)?;
        if trigger_mode(pin) == Some(x86_apic::IoApicTriggerMode::Level) {
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

unittest::path_mod_test!(mod tests, "../../ut-tests/x86.rs");
unittest::path_mod_test!(mod acpi_madt_tests, "../../ut-tests/acpi_madt.rs");

#[kiface::provide]
impl kirq::IntrManagerIf {
    fn guest_interface() -> Option<kirq::GuestIrqInterface> {
        None
    }

    fn percpu_forwarding(_: kirq::IrqDesc) -> Option<&'static kirq::PerCpuForwardingChip> {
        None
    }

    fn percpu_nmi(_: kirq::IrqDesc) -> Option<&'static kirq::PerCpuNmiChip> {
        None
    }

    fn select_flow(desc: kirq::IrqDesc) -> Result<kirq::IrqFlow, kirq::IrqDescError> {
        let flow = select_x86_flow(desc, x86_apic::pin_trigger_mode)?;
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

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::dispatch_irq`]'s contract.
    unsafe fn dispatch_irq(vector: usize) -> Option<kirq::Virq> {
        // SAFETY: the captured vector is claimed and consumed exactly once
        // within the facade caller's hardirq and CPU/IRQ exclusion.
        let pending = unsafe { dispatch_irq(vector)? };
        kirq::generic_handle_irq(pending)
    }

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::dispatch_nmi`]'s contract.
    unsafe fn dispatch_nmi(_irq: usize) {}

    /// # Safety
    /// The caller must uphold [`kirq::IntrManagerIf::complete_irq`]'s contract.
    unsafe fn complete_irq(completion_cookie: usize) {
        complete_delivery(completion_cookie, x86_apic::end_of_interrupt);
    }

    fn notify_cpu(interrupt_id: usize, target: kirq::TargetCpu) {
        notify_cpu(interrupt_id, target);
    }

    fn set_prio(_irq: usize, _priority: u8) {
        debug!("x86 APIC priority programming is not implemented");
    }
}
