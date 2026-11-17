// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Boot-time x86 IRQ configuration, validation, and publication.
//!
//! This module owns the immutable description of the IO-APIC topology and the
//! once-only transition from firmware declarations to installed controllers.
//! Runtime descriptor operations live in the parent module's `runtime`
//! submodule and read only the published topology table.

use fwnode::FwSource;
use kerrno::{KError, KResult};
use kirq::IrqTrigger;
use lazyinit::LazyInit;
use memaddr::pa;
use x86_apic::{IoApicInstanceId, IoApicPin};

pub const IO_APIC_DOMAIN: kirq::IrqDomainId = kirq::IO_APIC_DOMAIN;

/// Maximum number of IO-APIC controller instances this integration supports.
pub const MAX_IO_APICS: usize = x86_apic::MAX_IO_APICS;

/// Number of legacy ISA interrupt lines (sources 0..=15).
pub const MAX_LEGACY_IRQS: usize = 16;

/// One IO-APIC controller declaration consumed by the unified entry.
///
/// `id` is the firmware identity and is not used for routing yet. `gsi_base`
/// is the controller's first GSI; a GSI is translated to a local pin with
/// `gsi - gsi_base` after hardware probe.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicDeclaration {
    pub id: u8,
    pub address: u32,
    pub gsi_base: u32,
}

/// Where an effective legacy route came from.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum LegacyRouteOrigin {
    /// Platform identity default: `ISA n -> GSI n`, edge/high.
    Identity,
    /// Firmware interrupt-source override.
    Override,
}

/// Effective routing for one legacy ISA interrupt line.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct LegacyRoute {
    /// ISA line this route starts from (`0..=15`).
    pub isa_irq: u8,
    /// Global system interrupt this line resolves to.
    pub gsi: u32,
    /// Firmware trigger and implied polarity.
    pub trigger: IrqTrigger,
    /// Whether this is the identity default or a firmware override.
    pub origin: LegacyRouteOrigin,
}

/// One controller declaration and the `kirq` domain that identifies it.
///
/// The first instance reuses [`IO_APIC_DOMAIN`]. Every additional instance
/// has a distinct, non-identity domain, so equal local pin numbers on two
/// controllers never denote the same source.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct IoApicInstanceConfig {
    pub declaration: IoApicDeclaration,
    pub domain: kirq::IrqDomainId,
}

/// Inputs for the one x86 interrupt-initialization entry.
///
/// This is intentionally limited to what the current runtime consumes; it is
/// not the firmware-discovery container. `routes` is the effective ISA ->
/// GSI table shared by every controller instance. Controller order is
/// installation order, which is also the backend instance identifier order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct X86IrqInitConfig {
    pub policy: x86_apic::IoApicInitPolicy,
    pub source: FwSource,
    controllers: [Option<IoApicInstanceConfig>; MAX_IO_APICS],
    controller_count: usize,
    routes: [Option<LegacyRoute>; MAX_LEGACY_IRQS],
}

impl X86IrqInitConfig {
    /// Builds a single-controller configuration from firmware discovery.
    pub const fn from_acpi(
        controller: IoApicDeclaration,
        policy: x86_apic::IoApicInitPolicy,
    ) -> Self {
        Self::with_primary(controller, policy, FwSource::Acpi)
    }

    /// Builds a single-controller configuration from platform policy.
    pub const fn platform_static(
        controller: IoApicDeclaration,
        policy: x86_apic::IoApicInitPolicy,
    ) -> Self {
        Self::with_primary(controller, policy, FwSource::Static)
    }

    const fn with_primary(
        controller: IoApicDeclaration,
        policy: x86_apic::IoApicInitPolicy,
        source: FwSource,
    ) -> Self {
        let mut controllers = [None; MAX_IO_APICS];
        controllers[0] = Some(IoApicInstanceConfig {
            declaration: controller,
            domain: IO_APIC_DOMAIN,
        });
        Self {
            policy,
            source,
            controllers,
            controller_count: 1,
            routes: identity_legacy_routes(),
        }
    }

    /// Builds a multi-controller configuration from firmware discovery.
    ///
    /// The first declaration reuses [`IO_APIC_DOMAIN`]; each additional
    /// instance allocates its own non-identity domain. Firmware discovery has
    /// no namespace node for an IO-APIC, so allocation does not fabricate one.
    ///
    /// # Errors
    ///
    /// [`KError::InvalidInput`] when the declaration list is empty or exceeds
    /// [`MAX_IO_APICS`], and [`KError::NoMemory`] when an additional instance
    /// domain cannot be allocated.
    pub fn from_acpi_controllers(
        controllers: &[IoApicDeclaration],
        policy: x86_apic::IoApicInitPolicy,
    ) -> KResult<Self> {
        if controllers.is_empty() || controllers.len() > MAX_IO_APICS {
            return Err(KError::InvalidInput);
        }
        let mut config = Self {
            policy,
            source: FwSource::Acpi,
            controllers: [None; MAX_IO_APICS],
            controller_count: 0,
            routes: identity_legacy_routes(),
        };
        for (index, declaration) in controllers.iter().copied().enumerate() {
            let domain = if index == 0 {
                IO_APIC_DOMAIN
            } else {
                kirq::alloc_irq_domain(kirq::IrqController::IoApic).ok_or(KError::NoMemory)?
            };
            config.controllers[index] = Some(IoApicInstanceConfig {
                declaration,
                domain,
            });
            config.controller_count += 1;
        }
        Ok(config)
    }

    /// Configured controller instances in installation order.
    pub fn controllers(&self) -> impl Iterator<Item = IoApicInstanceConfig> + '_ {
        self.controllers[..self.controller_count]
            .iter()
            .flatten()
            .copied()
    }

    /// Number of configured controller instances.
    pub const fn controller_count(&self) -> usize {
        self.controller_count
    }

    /// Returns the effective route for one ISA line, if any.
    pub fn legacy_route(&self, isa_irq: u8) -> Option<LegacyRoute> {
        self.routes.get(usize::from(isa_irq)).copied().flatten()
    }

    /// Resolves a GSI to its controller instance and controller-local offset.
    ///
    /// Pre-probe selection uses only declared GSI bases and the backend's
    /// addressable pin bound. Boot validation repeats the resolution with the
    /// probed pin counts before any controller is programmed; overlapping
    /// controller ranges are rejected, so a valid configuration is
    /// unambiguous.
    pub(crate) fn controller_for_gsi(&self, gsi: u32) -> Option<(IoApicInstanceConfig, usize)> {
        let mut selected: Option<(IoApicInstanceConfig, usize)> = None;
        for instance in self.controllers() {
            let Some(offset) = gsi.checked_sub(instance.declaration.gsi_base) else {
                continue;
            };
            let Ok(offset) = usize::try_from(offset) else {
                continue;
            };
            if IoApicPin::try_from_usize(offset).is_none() {
                continue;
            }
            if selected.as_ref().is_none_or(|(current, _)| {
                instance.declaration.gsi_base > current.declaration.gsi_base
            }) {
                selected = Some((instance, offset));
            }
        }
        selected
    }

    /// Builds a descriptor request for a routed legacy ISA line.
    ///
    /// The descriptor carries the owning instance's domain and the
    /// controller-local pin as `hwirq`, never the GSI or ISA line. `None`
    /// means the line has no effective route, or its GSI lies outside every
    /// declared controller range this runtime can address. Hardware-bound
    /// pin-count checks are performed by [`init`] and by each backend
    /// operation.
    pub fn legacy_irq_desc(&self, isa_irq: u8) -> Option<kirq::IrqDesc> {
        let route = self.legacy_route(isa_irq)?;
        let (instance, pin) = self.controller_for_gsi(route.gsi)?;
        Some(
            kirq::IrqDesc::new(pin, route.trigger)
                .with_source(irq_source(self.source))
                .with_controller(kirq::IrqController::IoApic)
                .with_domain(instance.domain),
        )
    }

    /// Applies Linux's identity-invalidation rule after a firmware override.
    ///
    /// Dispatching `ISA x` to GSI `x` would alias the line that still uses its
    /// identity route to the same GSI, so that identity route is removed. A
    /// later explicit override for the target line still wins.
    pub(crate) fn invalidate_identity_route(&mut self, isa_irq: u8) {
        if let Some(slot) = self.routes.get_mut(usize::from(isa_irq))
            && slot
                .as_ref()
                .is_some_and(|route| route.origin == LegacyRouteOrigin::Identity)
        {
            *slot = None;
        }
    }

    /// Replaces one ISA line's route with a firmware override.
    pub(crate) fn set_legacy_override(&mut self, isa_irq: u8, gsi: u32, trigger: IrqTrigger) {
        if let Some(slot) = self.routes.get_mut(usize::from(isa_irq)) {
            *slot = Some(LegacyRoute {
                isa_irq,
                gsi,
                trigger,
                origin: LegacyRouteOrigin::Override,
            });
        }
    }
}

const fn identity_legacy_routes() -> [Option<LegacyRoute>; MAX_LEGACY_IRQS] {
    let mut routes = [None; MAX_LEGACY_IRQS];
    let mut isa_irq = 0;
    while isa_irq < MAX_LEGACY_IRQS {
        routes[isa_irq] = Some(LegacyRoute {
            isa_irq: isa_irq as u8,
            gsi: isa_irq as u32,
            trigger: IrqTrigger::EdgeRising,
            origin: LegacyRouteOrigin::Identity,
        });
        isa_irq += 1;
    }
    routes
}

const fn irq_source(source: FwSource) -> kirq::IrqSource {
    match source {
        FwSource::DeviceTree => kirq::IrqSource::DeviceTree,
        FwSource::Acpi => kirq::IrqSource::Acpi,
        FwSource::Static => kirq::IrqSource::PlatformStatic,
        FwSource::Unknown => kirq::IrqSource::Unknown,
    }
}

/// Rejects configurations that cannot be routed before touching hardware.
///
/// Controller addresses must be nonzero and unique. GSI range overlap needs
/// the probed pin counts, so [`validate_ranges`] performs it before any
/// redirection table is programmed.
pub(crate) fn validate_config(config: &X86IrqInitConfig) -> KResult<()> {
    if config.controller_count == 0 {
        return Err(KError::InvalidInput);
    }
    // Provenance is part of the contract: an unknown source cannot be
    // attributed to platform policy or firmware discovery.
    if config.source == FwSource::Unknown {
        return Err(KError::InvalidInput);
    }
    for (index, instance) in config.controllers().enumerate() {
        if instance.declaration.address == 0 {
            return Err(KError::InvalidInput);
        }
        for other in config.controllers().take(index) {
            if other.declaration.address == instance.declaration.address {
                return Err(KError::InvalidInput);
            }
        }
    }
    Ok(())
}

/// Rejects controllers whose pin window this runtime cannot represent.
pub(crate) fn validate_capabilities(
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

/// Rejects overlapping controller GSI ranges before any hardware programming.
///
/// Ranges are half-open `[gsi_base, gsi_base + pin_count)`. A GSI must name at
/// most one controller, otherwise a route has no unambiguous pin. This is
/// distinct from duplicate firmware overrides (last-wins) and from legitimate
/// shared use of one GSI by two ISA lines, both of which remain allowed.
pub(crate) fn validate_ranges(
    config: &X86IrqInitConfig,
    capabilities: &[x86_apic::IoApicCapabilities],
) -> KResult<()> {
    if capabilities.len() != config.controller_count {
        return Err(KError::InvalidInput);
    }
    for (index, instance) in config.controllers().enumerate() {
        let base = instance.declaration.gsi_base;
        let Some(end) = base.checked_add(u32::from(capabilities[index].pin_count)) else {
            return Err(KError::Unsupported);
        };
        for ((other_index, other), other_capabilities) in
            config.controllers().enumerate().zip(capabilities)
        {
            if other_index >= index {
                break;
            }
            let other_base = other.declaration.gsi_base;
            let Some(other_end) = other_base.checked_add(u32::from(other_capabilities.pin_count))
            else {
                return Err(KError::Unsupported);
            };
            if base < other_end && other_base < end {
                warn!(
                    "IO-APIC {} range [{}, {}) overlaps IO-APIC {} range [{}, {})",
                    instance.declaration.id, base, end, other.declaration.id, other_base, other_end
                );
                return Err(KError::InvalidInput);
            }
        }
    }
    Ok(())
}

/// Rejects explicit firmware routes the probed controllers cannot resolve.
///
/// Identity defaults are best-effort: a line whose identity GSI is outside
/// every probed range simply has no route. An explicit ISO is a firmware
/// assertion, so failing to honor it aborts boot instead of silently
/// reverting to identity. The pre-probe descriptor selection must agree with
/// the probed owner, because consumers may already retain that descriptor.
pub(crate) fn validate_legacy_routes(
    config: &X86IrqInitConfig,
    capabilities: &[x86_apic::IoApicCapabilities],
) -> KResult<()> {
    if capabilities.len() != config.controller_count {
        return Err(KError::InvalidInput);
    }
    for route in config.routes.iter().flatten() {
        if route.origin == LegacyRouteOrigin::Identity {
            continue;
        }
        let mut owner = None;
        for (instance, capabilities) in config.controllers().zip(capabilities) {
            if x86_apic::pin_for_gsi_in_range(
                route.gsi,
                instance.declaration.gsi_base,
                capabilities.pin_count,
            )
            .is_some()
            {
                if owner.is_some() {
                    return Err(KError::InvalidInput);
                }
                owner = Some(instance);
            }
        }
        let descriptor_owner = config
            .controller_for_gsi(route.gsi)
            .map(|(instance, _)| instance);
        if owner.is_none() || owner != descriptor_owner {
            warn!(
                "ISA IRQ {} routed to GSI {} is outside the probed IO-APIC ranges",
                route.isa_irq, route.gsi
            );
            return Err(KError::InvalidInput);
        }
    }
    Ok(())
}

/// One installed controller instance in the published topology.
///
/// `gsi_base` and `pin_count` describe the half-open GSI range
/// `[gsi_base, gsi_base + pin_count)` resolved by [`gsi_desc`]. `pin_count`
/// is the probed hardware size, not the backend's addressable pin bound.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct IoApicTopologyEntry {
    pub(crate) domain: kirq::IrqDomainId,
    pub(crate) gsi_base: u32,
    pub(crate) pin_count: u16,
}

/// Installed controller topology, indexed by backend instance id.
///
/// Published once after every controller is installed. Dispatch and GSI
/// resolution read this immutable table without a lock; it is the only
/// source of truth for instance domains and GSI ranges.
static IO_APIC_TOPOLOGY: LazyInit<[Option<IoApicTopologyEntry>; MAX_IO_APICS]> = LazyInit::new();

/// Resolves a descriptor domain to its controller instance.
pub(crate) fn instance_for_domain(domain: kirq::IrqDomainId) -> Option<IoApicInstanceId> {
    let index = IO_APIC_TOPOLOGY
        .get()?
        .iter()
        .position(|entry| entry.is_some_and(|entry| entry.domain == domain))?;
    IoApicInstanceId::try_from_usize(index)
}

/// Returns the published domain of one installed controller instance.
pub(crate) fn domain_for_instance(instance: IoApicInstanceId) -> Option<kirq::IrqDomainId> {
    IO_APIC_TOPOLOGY
        .get()?
        .get(instance.as_usize())?
        .map(|entry| entry.domain)
}

/// Resolves a firmware-described GSI to an IO-APIC descriptor.
///
/// Scans the published topology, performs the checked subtraction
/// `gsi - gsi_base`, and bounds the offset by the probed pin count. The
/// returned descriptor names the owning instance's domain and the
/// controller-local pin as `hwirq`; it never carries the GSI as a hardware
/// IRQ. Returns `None` before publication, for a GSI no instance owns, or for
/// a pin outside the probed window. Allocation-free and bounded by
/// [`MAX_IO_APICS`].
pub fn gsi_desc(gsi: u32, trigger: IrqTrigger) -> Option<kirq::IrqDesc> {
    gsi_desc_in(IO_APIC_TOPOLOGY.get()?, gsi, trigger)
}

/// Topology-injected [`gsi_desc`] used by production and tests.
pub(crate) fn gsi_desc_in(
    topology: &[Option<IoApicTopologyEntry>],
    gsi: u32,
    trigger: IrqTrigger,
) -> Option<kirq::IrqDesc> {
    let (entry, pin) = topology.iter().flatten().find_map(|entry| {
        x86_apic::pin_for_gsi_in_range(gsi, entry.gsi_base, entry.pin_count).map(|pin| (entry, pin))
    })?;
    Some(
        kirq::IrqDesc::new(pin.as_usize(), trigger)
            .with_source(kirq::IrqSource::Acpi)
            .with_controller(kirq::IrqController::IoApic)
            .with_domain(entry.domain),
    )
}

/// Initializes the boot CPU's Local APIC and every declared IO-APIC.
///
/// Pure declaration checks run first. Every controller is then probed without
/// programming, and capability, range-overlap and explicit-route validation
/// complete before the first redirection table is written. On failure the
/// caller must abort boot because already probed MMIO mappings are not
/// rolled back.
///
/// # Errors
///
/// [`KError::InvalidInput`] for an empty set, a zero or duplicate address,
/// unknown provenance, overlapping GSI ranges, or an explicit firmware route
/// outside the probed ranges; [`KError::Unsupported`] for an unrepresentable
/// pin window; and the backend's errors for mapping, instance capacity or
/// Local APIC owner allocation.
///
/// # Panics
///
/// Panics on duplicate Local APIC or IO-APIC publication, as required by the
/// once-only boot contract.
pub fn init(config: X86IrqInitConfig) -> KResult<()> {
    validate_config(&config)?;
    for instance in config.controllers() {
        info!(
            "Initialize x86 IRQs: IO-APIC {} at {:#010x}, gsi_base={}, domain={:?}, source={:?}",
            instance.declaration.id,
            instance.declaration.address,
            instance.declaration.gsi_base,
            instance.domain,
            config.source
        );
    }
    x86_apic::init_local_primary()?;

    // Probe every controller and validate the complete topology before any
    // redirection table is programmed.
    let mut probed: [Option<x86_apic::ProbedIoApic>; MAX_IO_APICS] = core::array::from_fn(|_| None);
    let mut capabilities = [x86_apic::IoApicCapabilities { pin_count: 0 }; MAX_IO_APICS];
    for (index, instance) in config.controllers().enumerate() {
        let declaration = instance.declaration;
        let probed_instance =
            x86_apic::probe_io_apic(pa!(declaration.address as usize), declaration.gsi_base)?;
        validate_capabilities(&declaration, probed_instance.capabilities())?;
        capabilities[index] = probed_instance.capabilities();
        probed[index] = Some(probed_instance);
    }
    let capabilities = &capabilities[..config.controller_count];
    validate_ranges(&config, capabilities)?;
    validate_legacy_routes(&config, capabilities)?;

    for (index, slot) in probed.iter_mut().take(config.controller_count).enumerate() {
        let instance = slot.take().ok_or(KError::InvalidInput)?;
        let installed = x86_apic::install_io_apic(instance, config.policy)?;
        if installed.as_usize() != index {
            return Err(KError::InvalidInput);
        }
    }

    let mut topology = [None; MAX_IO_APICS];
    for (index, instance) in config.controllers().enumerate() {
        topology[index] = Some(IoApicTopologyEntry {
            domain: instance.domain,
            gsi_base: instance.declaration.gsi_base,
            pin_count: capabilities[index].pin_count,
        });
    }
    IO_APIC_TOPOLOGY.init_once(topology);
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
