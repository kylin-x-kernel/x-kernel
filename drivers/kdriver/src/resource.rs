// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host backend and device-managed helpers for the [`device_res`] abstraction.
//!
//! The OS-agnostic resource model in [`device_res`] describes *what* a driver
//! needs; this module binds those operations to x-kernel's MMIO mapping, IRQ
//! manager, and coherent DMA allocator, and installs the backend during driver
//! init. For IRQs, this module is the adapter between `device_res` vocabulary
//! and `kirq`; the IRQ core does not depend on devres.
//!
//! The `devm_*` helpers acquire a resource and tie its lifetime to a
//! [`DeviceObject`] via its devres cleanup list, so a failed probe or a later
//! removal releases the resource automatically and in reverse acquisition
//! order.

use alloc::{sync::Arc, vec, vec::Vec};
use core::{
    alloc::Layout,
    array,
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use device_res::{
    DmaAllocation, DmaDirection, DmaMapping, DmaOp, DmaSpec, IrqController as DevIrqController,
    IrqEvent as DevIrqEvent, IrqHandler as DevIrqHandler, IrqHandlerToken, IrqOp, IrqResource,
    IrqRouteDesc, IrqTrigger as DevIrqTrigger, MmioMapping, MmioOp, MmioRegion, MsiResource,
    ResError, ResResult,
};
use driver_base::{DriverError, DriverResult};
use kdevice::DeviceObject;
use kspin::SpinNoIrq;

/// x-kernel implementation of the OS-agnostic resource provider.
struct HostResourceProvider;

static HOST_PROVIDER: HostResourceProvider = HostResourceProvider;

const IRQ_MAX_SHARED_HANDLERS: usize = 4;

struct IrqLine {
    virq: usize,
    state: Arc<IrqLineState>,
}

struct IrqLineState {
    handlers: SpinNoIrq<Vec<IrqLineHandler>>,
}

struct IrqLineHandler {
    token: IrqHandlerToken,
    handler: Arc<dyn DevIrqHandler>,
}

impl IrqLineState {
    fn dispatch(&self, virq: usize) -> DevIrqEvent {
        let (handlers, handler_count) = {
            let registered = self.handlers.lock();
            let mut handlers: [Option<Arc<dyn DevIrqHandler>>; IRQ_MAX_SHARED_HANDLERS] =
                array::from_fn(|_| None);
            for (dst, src) in handlers.iter_mut().zip(registered.iter()) {
                *dst = Some(src.handler.clone());
            }
            (handlers, registered.len())
        };

        let mut event = DevIrqEvent::NOT_HANDLED;
        for handler in handlers.into_iter().take(handler_count).flatten() {
            event.merge(handler.handle());
        }
        if event.handled() {
            irq_notify::dispatch_sources(virq, event.sources());
        }
        event
    }
}

static IRQ_LINES: SpinNoIrq<Vec<IrqLine>> = SpinNoIrq::new(Vec::new());
static NEXT_IRQ_HANDLER_TOKEN: AtomicUsize = AtomicUsize::new(1);

fn next_irq_handler_token() -> ResResult<IrqHandlerToken> {
    NEXT_IRQ_HANDLER_TOKEN
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
            current.checked_add(1)
        })
        .map(IrqHandlerToken::new)
        .map_err(|_| ResError::NoMemory)
}

impl MmioOp for HostResourceProvider {
    fn map_mmio(&self, region: MmioRegion, name: &'static str) -> ResResult<MmioMapping> {
        let vaddr =
            memspace::iomap_device(region.base.into(), region.size, name).map_err(map_iomap_err)?;
        let ptr = NonNull::new(vaddr.as_mut_ptr()).ok_or(ResError::MappingFailed)?;
        Ok(MmioMapping {
            vaddr: ptr.as_ptr() as usize,
            region,
        })
    }

    fn unmap_mmio(&self, mapping: MmioMapping) {
        let vaddr = memaddr::VirtAddr::from(mapping.vaddr);
        let _ = memspace::iounmap(vaddr);
    }
}

impl DmaOp for HostResourceProvider {
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation> {
        let layout =
            Layout::from_size_align(spec.len, spec.align).map_err(|_| ResError::InvalidResource)?;
        // SAFETY: `layout` is a valid non-zero layout and the returned buffer is
        // owned exclusively by the `DmaCoherent` handle that wraps this
        // allocation until it is freed via `free_coherent`.
        let info = unsafe { kdma::allocate_dma_memory(layout) }.map_err(|_| ResError::NoMemory)?;
        Ok(DmaAllocation {
            cpu_addr: info.cpu_addr.as_ptr() as usize,
            bus_addr: info.bus_addr.as_u64(),
            spec,
        })
    }

    fn free_coherent(&self, alloc: DmaAllocation) {
        let Ok(layout) = Layout::from_size_align(alloc.spec.len, alloc.spec.align) else {
            return;
        };
        let info = kdma::DMAInfo {
            cpu_addr: NonNull::new(alloc.cpu_addr as *mut u8)
                .expect("coherent DMA allocation stored a null CPU address"),
            bus_addr: kdma::DmaBusAddress::new(alloc.bus_addr),
        };
        // SAFETY: `info` and `layout` describe a coherent buffer previously
        // returned by `alloc_coherent` for the same spec, and it is freed
        // exactly once when its owning handle is dropped.
        unsafe { kdma::deallocate_dma_memory(info, layout) };
    }

    fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        direction: DmaDirection,
    ) -> ResResult<DmaMapping> {
        let dir = map_dma_direction(direction);
        // SAFETY: the caller guarantees `buffer` is a valid, live slice for the
        // duration of the mapping.
        let slice: &[u8] = unsafe { buffer.as_ref() };
        let len = slice.len();
        let cpu_addr = NonNull::from(slice).cast::<u8>();
        // SAFETY: same contract — `buffer` is valid for the mapping duration.
        let info = unsafe { kdma::map_dma_buffer(buffer, dir) }.map_err(|_| ResError::NoMemory)?;
        Ok(DmaMapping {
            cpu_addr: cpu_addr.as_ptr() as usize,
            bus_addr: info.bus_addr.as_u64(),
            len,
            direction,
        })
    }

    fn unmap_streaming(&self, mapping: DmaMapping) {
        let cpu_addr = NonNull::new(mapping.cpu_addr as *mut u8)
            .expect("streaming DMA mapping stored a null CPU address");
        let buffer = NonNull::slice_from_raw_parts(cpu_addr, mapping.len);
        // SAFETY: `mapping` describes a streaming mapping previously established
        // by `map_streaming`; `cpu_addr` + `len` reconstruct the original buffer.
        unsafe {
            kdma::unmap_dma_buffer(
                kdma::DmaBusAddress::new(mapping.bus_addr),
                buffer,
                map_dma_direction(mapping.direction),
            );
        }
    }
}

impl IrqOp for HostResourceProvider {
    fn request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> ResResult<IrqHandlerToken> {
        let mut lines = IRQ_LINES.lock();
        if let Some(line) = lines.iter().find(|line| line.virq == irq.number) {
            let mut handlers = line.state.handlers.lock();
            if handlers.len() >= IRQ_MAX_SHARED_HANDLERS {
                return Err(ResError::Busy);
            }
            let token = next_irq_handler_token()?;
            handlers.push(IrqLineHandler { token, handler });
            return Ok(token);
        }

        let token = next_irq_handler_token()?;
        let state = Arc::new(IrqLineState {
            handlers: SpinNoIrq::new(vec![IrqLineHandler { token, handler }]),
        });
        let dispatch_state = state.clone();
        let virq = irq.number;
        let dispatch_handler: Arc<dyn kirq::IrqHandler> =
            Arc::new(move || map_devres_irq_event(dispatch_state.dispatch(virq)));
        match kirq::try_register(map_irq_resource_to_desc(irq), dispatch_handler) {
            Ok(true) => {}
            Ok(false) => return Err(ResError::Busy),
            Err(err) => {
                log::warn!("failed to register IRQ handler for {irq:?}: {err:?}");
                return Err(map_irq_desc_error(err));
            }
        }
        lines.push(IrqLine { virq, state });
        Ok(token)
    }

    fn release_irq(&self, irq: IrqResource, token: IrqHandlerToken) {
        let mut lines = IRQ_LINES.lock();
        let Some(line_index) = lines.iter().position(|line| line.virq == irq.number) else {
            return;
        };
        let mut handlers = lines[line_index].state.handlers.lock();
        let Some(handler_index) = handlers.iter().position(|handler| handler.token == token) else {
            return;
        };
        handlers.swap_remove(handler_index);
        if !handlers.is_empty() {
            return;
        }
        drop(handlers);
        lines.swap_remove(line_index);
        if kirq::unregister(irq.number).is_none() {
            log::warn!("failed to unregister IRQ {}: no handler found", irq.number);
        }
    }

    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool) {
        kirq::enable(map_irq_resource_to_desc(irq), enabled);
    }

    fn map_irq(&self, route: IrqRouteDesc) -> ResResult<IrqResource> {
        let domain = map_irq_domain(route.controller);
        let mut desc = kirq::IrqDesc::new(route.hwirq, map_irq_trigger(route.trigger))
            .with_controller(map_irq_controller(route.controller));
        if let Some(domain) = domain {
            desc = desc.with_domain(domain);
        }
        let virq = kirq::try_map(desc).map_err(|err| {
            log::warn!("failed to map IRQ route {route:?}: {err:?}");
            map_irq_desc_error(err)
        })?;
        Ok(IrqResource::new(virq, route.trigger)
            .with_controller(route.controller)
            .with_hwirq(route.hwirq))
    }

    fn alloc_msix(&self) -> ResResult<MsiResource> {
        #[cfg(target_arch = "x86_64")]
        {
            let allocation = kirq::alloc_msix(kirq::IrqAffinity::Any).ok_or(ResError::NoMemory)?;
            let irq = IrqResource::new(allocation.virq(), DevIrqTrigger::EdgeRising)
                .with_controller(DevIrqController::Unknown);
            let message = allocation.message();
            Ok(MsiResource::new(
                irq,
                device_res::MsiMessage::new(message.address(), message.data()),
            ))
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            Err(ResError::Unsupported)
        }
    }

    fn free_msix(&self, resource: MsiResource) {
        #[cfg(target_arch = "x86_64")]
        {
            if !kirq::free_msix(resource.irq.number) {
                log::warn!(
                    "failed to free MSI-X IRQ {} (not allocated?)",
                    resource.irq.number
                );
            }
        }

        #[cfg(not(target_arch = "x86_64"))]
        {
            let _ = resource;
        }
    }
}

/// Install the x-kernel resource provider backend.
///
/// Must run before any driver acquires a resource. It is installed before the
/// platform `early_driver_init` hook so that even early device interrupts (such
/// as the console input line) flow through the resource provider rather than
/// `khal` directly.
pub fn install_resource_provider() {
    device_res::set_mmio_provider(&HOST_PROVIDER);
    device_res::set_dma_provider(&HOST_PROVIDER);
    device_res::set_irq_provider(&HOST_PROVIDER);
}

/// Translate a device-res DMA direction into the matching `kdma` direction.
fn map_dma_direction(d: DmaDirection) -> kdma::DmaDirection {
    match d {
        DmaDirection::DriverToDevice => kdma::DmaDirection::DriverToDevice,
        DmaDirection::DeviceToDriver => kdma::DmaDirection::DeviceToDriver,
        DmaDirection::Bidirectional => kdma::DmaDirection::Bidirectional,
    }
}

fn map_iomap_err(err: memspace::IoMapError) -> ResError {
    match err {
        memspace::IoMapError::NoMemory => ResError::NoMemory,
        memspace::IoMapError::InvalidRange => ResError::InvalidResource,
        memspace::IoMapError::MappingFailed => ResError::MappingFailed,
    }
}

fn map_irq_desc_error(err: kirq::IrqDescError) -> ResError {
    match err {
        kirq::IrqDescError::VirqExhausted { .. } => ResError::NoMemory,
        kirq::IrqDescError::HwirqConflict { .. }
        | kirq::IrqDescError::DomainConflict { .. }
        | kirq::IrqDescError::VirqConflict { .. }
        | kirq::IrqDescError::MappingConflict { .. }
        | kirq::IrqDescError::UnknownDomain { .. } => ResError::InvalidResource,
    }
}

fn map_res_err(err: ResError) -> DriverError {
    match err {
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed => DriverError::Io,
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Busy => DriverError::ResourceBusy,
        ResError::Unsupported => DriverError::Unsupported,
        ResError::NoProvider => DriverError::BadState,
    }
}

fn map_irq_trigger(trigger: DevIrqTrigger) -> kirq::IrqTrigger {
    match trigger {
        DevIrqTrigger::EdgeRising => kirq::IrqTrigger::EdgeRising,
        DevIrqTrigger::EdgeFalling => kirq::IrqTrigger::EdgeFalling,
        DevIrqTrigger::LevelHigh => kirq::IrqTrigger::LevelHigh,
        DevIrqTrigger::LevelLow => kirq::IrqTrigger::LevelLow,
        DevIrqTrigger::Unknown(flags) => kirq::IrqTrigger::Unknown(flags),
    }
}

fn map_irq_controller(controller: DevIrqController) -> kirq::IrqController {
    match controller {
        DevIrqController::Gic => kirq::IrqController::Gic,
        DevIrqController::Plic => kirq::IrqController::Plic,
        DevIrqController::IoApic => kirq::IrqController::IoApic,
        DevIrqController::LoongArchExtioi => kirq::IrqController::LoongArchExtioi,
        DevIrqController::Unknown => kirq::IrqController::Unknown,
    }
}

fn map_irq_domain(controller: DevIrqController) -> Option<kirq::IrqDomainId> {
    // `device_res::IrqDomainId` is provider-local. Only controllers with a
    // registered kirq domain can produce data-plane-resolvable mappings here.
    match controller {
        DevIrqController::Gic => Some(kirq::GIC_ROOT_DOMAIN),
        DevIrqController::Plic => Some(kirq::PLIC_ROOT_DOMAIN),
        DevIrqController::IoApic => Some(kirq::IO_APIC_DOMAIN),
        DevIrqController::LoongArchExtioi | DevIrqController::Unknown => None,
    }
}

fn map_irq_resource_to_desc(resource: IrqResource) -> kirq::IrqDesc {
    if resource.hwirq.is_none()
        && resource.domain.is_none()
        && resource.controller.unwrap_or(DevIrqController::Unknown) == DevIrqController::Unknown
    {
        return kirq::IrqDesc::from_virq(resource.number);
    }

    let hwirq = resource.hwirq.unwrap_or(resource.number);
    let controller = resource.controller.unwrap_or(DevIrqController::Unknown);
    let mut desc = kirq::IrqDesc::new(hwirq, map_irq_trigger(resource.trigger))
        .with_controller(map_irq_controller(controller))
        .with_virq(resource.number);
    if let Some(domain) = map_irq_domain(controller) {
        desc = desc.with_domain(domain);
    }
    desc
}

fn map_devres_irq_event(event: DevIrqEvent) -> kirq::IrqEvent {
    if event.handled() {
        kirq::IrqEvent::from_sources(event.sources())
    } else {
        kirq::IrqEvent::NOT_HANDLED
    }
}

/// Map an MMIO region and tie its lifetime to `device`.
///
/// The mapping is released when the device's probe fails or the device is
/// removed. Returns the virtual base address for the region.
pub fn devm_iomap(
    device: &DeviceObject,
    region: MmioRegion,
    name: &'static str,
) -> DriverResult<NonNull<u8>> {
    device_res::devm_iomap(device, region, name).map_err(map_res_err)
}

/// Register a context-carrying interrupt handler and tie its lifetime to
/// `device`.
///
/// The handler is released when the device's probe fails or the device is
/// removed.
pub fn devm_request_irq(
    device: &DeviceObject,
    irq: IrqResource,
    handler: Arc<dyn DevIrqHandler>,
) -> DriverResult<()> {
    device_res::devm_request_irq(device, irq, handler).map_err(map_res_err)
}

/// Allocate a coherent DMA buffer and tie its lifetime to `device`.
///
/// The buffer is freed when the device's probe fails or the device is removed.
/// Returns the CPU virtual address and device bus address of the buffer.
pub fn devm_alloc_coherent(
    device: &DeviceObject,
    spec: DmaSpec,
) -> DriverResult<(NonNull<u8>, u64)> {
    device_res::devm_alloc_coherent(device, spec).map_err(map_res_err)
}
