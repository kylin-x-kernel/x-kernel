// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host backend and device-managed helpers for the [`device_res`] abstraction.
//!
//! The OS-agnostic resource model in [`device_res`] describes *what* a driver
//! needs; this module binds those operations to x-kernel's MMIO mapping, IRQ
//! manager, and coherent DMA allocator, and installs the backend during driver
//! init.
//!
//! The `devm_*` helpers acquire a resource and tie its lifetime to a
//! [`DeviceObject`] via its devres cleanup list, so a failed probe or a later
//! removal releases the resource automatically and in reverse acquisition
//! order.

use alloc::sync::Arc;
use core::{alloc::Layout, ptr::NonNull};

use device_res::{
    DmaAllocation, DmaSpec, IrqHandler, IrqResource, MmioMapping, MmioRegion, ResError, ResResult,
    ResourceProvider,
};
use driver_base::{DriverError, DriverResult};
use kdevice::DeviceObject;
use kspin::SpinNoIrq;

/// x-kernel implementation of the OS-agnostic resource provider.
struct HostResourceProvider;

static HOST_PROVIDER: HostResourceProvider = HostResourceProvider;

/// Number of interrupt handlers that can be registered through the resource
/// provider at once. `khal` dispatches the regular IRQ handler as a bare
/// `fn()` carrying no identity, so each registration is bridged to a distinct
/// trampoline that recovers a context-carrying [`IrqHandler`] from this table.
const MAX_IRQ_SLOTS: usize = 64;

/// A single bridged interrupt registration.
struct IrqSlot {
    state: SpinNoIrq<Option<IrqSlotState>>,
}

struct IrqSlotState {
    virq: usize,
    handler: Arc<dyn IrqHandler>,
}

impl IrqSlot {
    const fn new() -> Self {
        Self {
            state: SpinNoIrq::new(None),
        }
    }
}

static IRQ_SLOTS: [IrqSlot; MAX_IRQ_SLOTS] = [const { IrqSlot::new() }; MAX_IRQ_SLOTS];

/// Invoke the context handler bound to `slot`, if any.
///
/// The owning `Arc` is cloned out from under the slot lock so the lock is not
/// held while the (potentially re-entrant) handler runs.
fn dispatch_slot(slot: usize) {
    let handler = IRQ_SLOTS[slot]
        .state
        .lock()
        .as_ref()
        .map(|state| state.handler.clone());
    if let Some(handler) = handler {
        let _ = handler.handle();
    }
}

/// Distinct `fn()` trampolines, one per slot, each forwarding to its slot.
macro_rules! irq_trampolines {
    ($($slot:literal),* $(,)?) => {
        [ $( || dispatch_slot($slot) ),* ]
    };
}

#[rustfmt::skip]
static IRQ_TRAMPOLINES: [fn(); MAX_IRQ_SLOTS] = irq_trampolines!(
     0,  1,  2,  3,  4,  5,  6,  7,  8,  9, 10, 11, 12, 13, 14, 15,
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
    32, 33, 34, 35, 36, 37, 38, 39, 40, 41, 42, 43, 44, 45, 46, 47,
    48, 49, 50, 51, 52, 53, 54, 55, 56, 57, 58, 59, 60, 61, 62, 63,
);

impl ResourceProvider for HostResourceProvider {
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

    fn request_irq(&self, irq: IrqResource, handler: Arc<dyn IrqHandler>) -> ResResult<()> {
        for (slot, trampoline) in IRQ_SLOTS.iter().zip(IRQ_TRAMPOLINES.iter()) {
            let mut guard = slot.state.lock();
            if guard.is_some() {
                continue;
            }
            // Store the handler before enabling the line so an interrupt that
            // fires immediately after `register` finds its handler in place.
            *guard = Some(IrqSlotState {
                virq: irq.number,
                handler,
            });
            if khal::irq::register(irq.number, *trampoline) {
                return Ok(());
            }
            *guard = None;
            return Err(ResError::Busy);
        }
        Err(ResError::NoMemory)
    }

    fn release_irq(&self, irq: IrqResource) {
        for slot in IRQ_SLOTS.iter() {
            let mut guard = slot.state.lock();
            if guard.as_ref().is_some_and(|state| state.virq == irq.number) {
                let _ = khal::irq::unregister(irq.number);
                *guard = None;
                return;
            }
        }
    }

    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool) {
        khal::irq::enable(irq.number, enabled);
    }

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
}

/// Install the x-kernel resource provider backend.
///
/// Must run before any driver acquires a resource. It is installed before the
/// platform `early_driver_init` hook so that even early device interrupts (such
/// as the console input line) flow through the resource provider rather than
/// `khal` directly.
pub fn install_resource_provider() {
    device_res::set_provider(&HOST_PROVIDER);
}

fn map_iomap_err(err: memspace::IoMapError) -> ResError {
    match err {
        memspace::IoMapError::NoMemory => ResError::NoMemory,
        memspace::IoMapError::InvalidRange => ResError::InvalidResource,
        memspace::IoMapError::MappingFailed => ResError::MappingFailed,
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
    handler: Arc<dyn IrqHandler>,
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
