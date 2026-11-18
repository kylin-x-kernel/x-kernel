// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! X-Kernel implementation of the [`driver_os_api::IrqOp`] provider contract.

use alloc::sync::Arc;

use driver_os_api::{
    IrqController as DevIrqController, IrqEvent as DevIrqEvent, IrqHandler as DevIrqHandler,
    IrqHandlerToken, IrqOp, IrqResource, IrqRouteDesc, IrqTrigger as DevIrqTrigger, MsiResource,
    ResError, ResResult, Virq,
};

use crate::XKernelResourceProvider;

impl IrqOp for XKernelResourceProvider {
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        let dispatch_handler: Arc<dyn kirq::IrqHandler> = Arc::new(move |virq| {
            let event = handler.handle(virq);
            dev_irq_event_to_kirq(event)
        });
        match kirq::try_register_shared(irq_resource_to_kirq_spec(irq), dispatch_handler) {
            Ok(Some(token)) => Ok(IrqHandlerToken::SharedAction(token.id())),
            Ok(None) => Err(ResError::Busy),
            Err(err) => {
                log::warn!("failed to register IRQ handler for {irq:?}: {err:?}");
                Err(map_irq_desc_error(err))
            }
        }
    }

    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken) {
        let result = match token {
            IrqHandlerToken::RegularAction => {
                kirq::try_free_irq(irq_resource_to_kirq_spec(irq)).map(|handler| handler.is_some())
            }
            IrqHandlerToken::SharedAction(id) => kirq::try_free_irq_action(
                irq_resource_to_kirq_spec(irq),
                kirq::IrqActionToken::new(id),
            )
            .map(|handler| handler.is_some()),
        };
        if let Err(error) = require_released_action(result) {
            #[expect(
                clippy::panic,
                reason = "CG-021-no-panic-on-reachable-failure: TODO(cg): infallible IRQ release \
                          cannot authorize resource reclamation after failed detach/drain; a \
                          fallible teardown must retain its owner on failure"
            )]
            {
                panic!("IRQ release did not establish quiescence: {irq:?} {token:?}: {error:?}");
            }
        }
    }

    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool) {
        kirq::enable(irq_resource_to_kirq_spec(irq), enabled);
    }

    fn map_fw_irq(&self, spec: fwnode::FwIrqSpec<'static>) -> ResResult<IrqResource> {
        let mapped = kirq::try_map_fwspec(spec).map_err(|err| {
            log::warn!("failed to map firmware IRQ declaration {spec:?}: {err:?}");
            map_fw_irq_error(err)
        })?;
        let desc = mapped.desc;
        Ok(IrqResource::mapped(
            Virq::new(mapped.virq),
            desc.trigger,
            desc.controller,
            desc.domain,
            desc.hwirq,
        ))
    }

    fn map_irq(&self, route: IrqRouteDesc) -> ResResult<IrqResource> {
        // hwirq is domain-local. An IO-APIC route without its instance domain
        // cannot say which controller the pin belongs to; mapping it by number
        // would silently turn a GSI into a pin (or a pin on the wrong
        // controller). ACPI callers must resolve the GSI through map_gsi.
        if route.controller == DevIrqController::IoApic && route.domain.is_none() {
            log::warn!("rejecting IO-APIC IRQ route without a resolved domain: {route:?}");
            return Err(ResError::InvalidResource);
        }
        let desc = irq_route_to_kirq_desc(route);
        let virq = kirq::try_map(desc).map_err(|err| {
            log::warn!("failed to map IRQ route {route:?}: {err:?}");
            map_irq_desc_error(err)
        })?;
        Ok(IrqResource::mapped(
            Virq::new(virq),
            route.trigger,
            route.controller,
            desc.domain,
            route.hwirq,
        ))
    }

    fn map_gsi(&self, gsi: u32, trigger: DevIrqTrigger) -> ResResult<IrqResource> {
        #[cfg(target_arch = "x86_64")]
        {
            map_gsi_with(irq_driver::x86::gsi_desc, gsi, trigger)
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = (gsi, trigger);
            Err(ResError::Unsupported)
        }
    }

    fn alloc_msix(&self) -> ResResult<MsiResource> {
        #[cfg(target_arch = "x86_64")]
        {
            let allocation = kirq::alloc_msix(kirq::IrqAffinity::Any).ok_or(ResError::NoMemory)?;
            let irq = IrqResource::plain(Virq::new(allocation.virq()), DevIrqTrigger::EdgeRising);
            let message = allocation.message();
            Ok(MsiResource::new(
                irq,
                driver_os_api::MsiMessage::new(message.address(), message.data()),
            ))
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            Err(ResError::Unsupported)
        }
    }

    /// Requires a watched, sleepable task with the RCU service running, plus
    /// the source-stop, action-drain and exclusion rules of IrqOp. KIRQ exposes
    /// only a bool here, so Busy includes context and identity rejection too.
    fn try_free_msix(&self, resource: MsiResource) -> Result<(), (ResError, MsiResource)> {
        #[cfg(target_arch = "x86_64")]
        {
            let is_released = kirq::free_msix(resource.irq.virq().raw());
            msix_release_result(resource, is_released)
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            Err((ResError::Unsupported, resource))
        }
    }
}

#[cfg(any(target_arch = "x86_64", unittest))]
fn msix_release_result(
    resource: MsiResource,
    is_released: bool,
) -> Result<(), (ResError, MsiResource)> {
    if is_released {
        Ok(())
    } else {
        Err((ResError::Busy, resource))
    }
}

/// Domain for legacy routes that name a controller family but no instance.
///
/// The roots stay reserved for the first firmware-registered GIC/PLIC, so this
/// fallback matches the primary controller. IO-APIC routes have no such root
/// fallback: a GSI must be resolved through `map_gsi`, which names the exact
/// instance. Firmware-described resources use `try_map_fwspec`, which resolves
/// the exact controller node and therefore also supports additional
/// same-family instances.
fn default_irq_domain(controller: DevIrqController) -> Option<kirq::IrqDomainId> {
    match controller {
        DevIrqController::Gic => Some(kirq::GIC_ROOT_DOMAIN),
        DevIrqController::Plic => Some(kirq::PLIC_ROOT_DOMAIN),
        DevIrqController::IoApic
        | DevIrqController::LoongArchExtioi
        | DevIrqController::Msi
        | DevIrqController::Unknown => None,
    }
}

/// Requires the x86 GSI resolver to own the GSI.
///
/// Injected so the provider's error contract is testable without a published
/// IO-APIC topology.
#[cfg(any(target_arch = "x86_64", unittest))]
fn resolve_gsi_desc(
    resolve: impl FnOnce(u32, DevIrqTrigger) -> Option<kirq::IrqDesc>,
    gsi: u32,
    trigger: DevIrqTrigger,
) -> ResResult<kirq::IrqDesc> {
    resolve(gsi, trigger).ok_or(ResError::InvalidResource)
}

/// Maps a resolved GSI descriptor and retains its exact origin.
///
/// The resolver is injected so the resolve -> `kirq::try_map` chain can be
/// exercised independently of a published x86 topology.
#[cfg(target_arch = "x86_64")]
fn map_gsi_with(
    resolve: impl FnOnce(u32, DevIrqTrigger) -> Option<kirq::IrqDesc>,
    gsi: u32,
    trigger: DevIrqTrigger,
) -> ResResult<IrqResource> {
    let desc = resolve_gsi_desc(resolve, gsi, trigger)?;
    let virq = kirq::try_map(desc).map_err(|err| {
        log::warn!("failed to map GSI {gsi}: {err:?}");
        map_irq_desc_error(err)
    })?;
    Ok(mapped_gsi_resource(desc, virq, trigger))
}

/// Builds the provider resource from the exact descriptor KIRQ mapped.
///
/// The controller-local pin and instance domain are copied from the resolver
/// result. Rebuilding them from the GSI or a controller-family default domain
/// would alias additional controllers onto the primary instance.
#[cfg(target_arch = "x86_64")]
fn mapped_gsi_resource(
    desc: kirq::IrqDesc,
    virq: kirq::Virq,
    trigger: DevIrqTrigger,
) -> IrqResource {
    IrqResource::mapped(
        Virq::new(virq),
        trigger,
        DevIrqController::IoApic,
        desc.domain,
        desc.hwirq,
    )
}

fn map_fw_irq_error(err: kirq::FwIrqMapError) -> ResError {
    match err {
        kirq::FwIrqMapError::ControllerNotReady => ResError::BadState,
        kirq::FwIrqMapError::MalformedSpec | kirq::FwIrqMapError::InvalidHwirq => {
            ResError::InvalidResource
        }
        kirq::FwIrqMapError::UnsupportedSpec => ResError::Unsupported,
        kirq::FwIrqMapError::Map(err) => map_irq_desc_error(err),
    }
}

fn irq_route_to_kirq_desc(route: IrqRouteDesc) -> kirq::IrqDesc {
    let mut desc = kirq::IrqDesc::new(route.hwirq, route.trigger).with_controller(route.controller);
    if let Some(domain) = route
        .domain
        .or_else(|| default_irq_domain(route.controller))
    {
        desc = desc.with_domain(domain);
    }
    desc
}

/// Handler registration and line control address a mapping that has already
/// happened. Rebuilding a descriptor from the resource would bypass the IRQ
/// core's authoritative metadata, so both shapes resolve by logical number.
fn irq_resource_to_kirq_spec(irq: IrqResource) -> kirq::IrqSpec {
    kirq::IrqSpec::PlainVirq(irq.virq().raw())
}

fn dev_irq_event_to_kirq(event: DevIrqEvent) -> kirq::IrqEvent {
    if event.handled() {
        kirq::IrqEvent::HANDLED
    } else {
        kirq::IrqEvent::NOT_HANDLED
    }
}

fn require_released_action(result: Result<bool, kirq::IrqDescError>) -> ResResult<()> {
    match result {
        Ok(true) => Ok(()),
        Ok(false) => Err(ResError::InvalidResource),
        Err(error) => Err(map_irq_desc_error(error)),
    }
}

fn map_irq_desc_error(err: kirq::IrqDescError) -> ResError {
    match err {
        kirq::IrqDescError::VirqExhausted { .. } | kirq::IrqDescError::NoMemory => {
            ResError::NoMemory
        }
        kirq::IrqDescError::NotReady | kirq::IrqDescError::InvalidCpu { .. } => ResError::BadState,
        kirq::IrqDescError::HwirqConflict { .. }
        | kirq::IrqDescError::DomainConflict { .. }
        | kirq::IrqDescError::TriggerConflict { .. }
        | kirq::IrqDescError::PolarityConflict { .. }
        | kirq::IrqDescError::ControllerConflict { .. }
        | kirq::IrqDescError::VirqConflict { .. }
        | kirq::IrqDescError::MappingConflict { .. }
        | kirq::IrqDescError::VirqMappingConflict { .. } => ResError::AlreadyExists,
        kirq::IrqDescError::InvalidHwirq { .. }
        | kirq::IrqDescError::UnsupportedFlow { .. }
        | kirq::IrqDescError::ConfigurationFailed { .. }
        | kirq::IrqDescError::UnknownDomain { .. }
        | kirq::IrqDescError::UnknownIrq
        | kirq::IrqDescError::TeardownInProgress { .. }
        | kirq::IrqDescError::NoIrqAction { .. }
        | kirq::IrqDescError::InvalidContext { .. } => ResError::InvalidResource,
    }
}

unittest::path_mod_test!(tests, "../ut-tests/irq.rs");
