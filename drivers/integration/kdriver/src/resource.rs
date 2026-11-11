// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver-facing resource helpers backed by the [`driver_os_api`] provider model.
//!
//! The X-Kernel provider implementation lives in `driver-os-api-xkernel`. This
//! module keeps the driver orchestration crate's `DriverResult` boundary while
//! acquiring resources through the OS-neutral `driver_os_api` API.

use alloc::sync::Arc;
use core::ptr::NonNull;

use driver_base::{DriverError, DriverResult};
use driver_os_api::{
    DmaSpec, IrqHandler as DevIrqHandler, IrqResource, IrqThreadHandler as DevIrqThreadHandler,
    MmioClaim, MmioRegion, ResError,
};
use driver_os_api_xkernel::XKernelResourceProvider;
use kdevice::DeviceObject;

static XKERNEL_RESOURCE_PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

pub(crate) fn resource_provider() -> &'static XKernelResourceProvider {
    &XKERNEL_RESOURCE_PROVIDER
}

pub(crate) fn map_res_err(err: ResError) -> DriverError {
    match err {
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed
        | ResError::Io
        | ResError::BadState
        | ResError::NotFound
        | ResError::Interrupted => DriverError::Io,
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Busy | ResError::AlreadyExists => DriverError::ResourceBusy,
        ResError::Unsupported => DriverError::Unsupported,
    }
}

/// Claim a physical MMIO range through the framework-owned provider.
///
/// The provider rejects any overlapping live claim with `ResourceBusy` before
/// the range is accessed. The returned guard releases
/// the range on drop; callers must retain it until their last register access
/// has ended. This does not arbitrate against code paths that do not use the
/// shared claim contract.
pub(crate) fn claim_mmio_region(region: MmioRegion, name: &'static str) -> DriverResult<MmioClaim> {
    MmioClaim::claim_with(resource_provider(), region, name).map_err(map_res_err)
}

/// Map an MMIO region and tie its lifetime to `device`.
///
/// The mapping is released when the device's probe fails or the device is
/// removed. Returns the virtual base address for the region.
pub(crate) fn devm_iomap(
    device: &DeviceObject,
    region: MmioRegion,
    name: &'static str,
) -> DriverResult<NonNull<u8>> {
    kdevice::device::managed::devm_iomap_with_provider(resource_provider(), device, region, name)
        .map_err(map_res_err)
}

/// Register a context-carrying interrupt handler and tie its lifetime to
/// `device`.
///
/// The handler is released when the device's probe fails or the device is
/// removed.
pub(crate) fn devm_request_irq(
    device: &DeviceObject,
    irq: IrqResource,
    handler: Arc<dyn DevIrqHandler>,
) -> DriverResult<()> {
    kdevice::device::managed::devm_request_irq_with_provider(
        resource_provider(),
        device,
        irq,
        handler,
    )
    .map_err(map_res_err)
}

/// Register primary and threaded interrupt handlers and tie their lifetime to
/// `device`.
///
/// The request is routed through the framework-owned `driver_os_api` IRQ provider.
pub(crate) fn devm_request_threaded_irq(
    device: &DeviceObject,
    irq: IrqResource,
    primary: Arc<dyn DevIrqHandler>,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    kdevice::device::managed::devm_request_threaded_irq_with_provider(
        resource_provider(),
        device,
        irq,
        primary,
        thread,
        name,
    )
    .map_err(map_res_err)
}

/// Register a default-primary threaded interrupt handler and tie its lifetime
/// to `device`.
///
/// The request is routed through the framework-owned `driver_os_api` IRQ provider.
pub(crate) fn devm_request_threaded_irq_default(
    device: &DeviceObject,
    irq: IrqResource,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    kdevice::device::managed::devm_request_threaded_irq_default_with_provider(
        resource_provider(),
        device,
        irq,
        thread,
        name,
    )
    .map_err(map_res_err)
}

/// Allocate a coherent DMA buffer and tie its lifetime to `device`.
///
/// The buffer is freed when the device's probe fails or the device is removed.
/// Returns the CPU virtual address and device bus address of the buffer.
pub(crate) fn devm_alloc_coherent(
    device: &DeviceObject,
    spec: DmaSpec,
) -> DriverResult<(NonNull<u8>, u64)> {
    kdevice::device::managed::devm_alloc_coherent_with_provider(resource_provider(), device, spec)
        .map_err(map_res_err)
}

/// Driver-facing device-managed resource methods.
///
/// Drivers use this extension trait to acquire resources from their
/// [`DeviceObject`] without naming the concrete host provider. The provider is
/// owned by the driver framework and passed explicitly into `driver_os_api`.
pub trait DeviceResourceExt {
    /// Map an MMIO region and tie its lifetime to this device.
    fn devm_iomap(&self, region: MmioRegion, name: &'static str) -> DriverResult<NonNull<u8>>;

    /// Register a context-carrying interrupt handler and tie its lifetime to
    /// this device.
    fn devm_request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> DriverResult<()>;

    /// Register primary and threaded interrupt handlers and tie their lifetime
    /// to this device.
    fn devm_request_threaded_irq(
        &self,
        irq: IrqResource,
        primary: Arc<dyn DevIrqHandler>,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()>;

    /// Register a default-primary threaded interrupt handler and tie its
    /// lifetime to this device.
    fn devm_request_threaded_irq_default(
        &self,
        irq: IrqResource,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()>;

    /// Allocate a coherent DMA buffer and tie its lifetime to this device.
    fn devm_alloc_coherent(&self, spec: DmaSpec) -> DriverResult<(NonNull<u8>, u64)>;
}

impl DeviceResourceExt for DeviceObject {
    fn devm_iomap(&self, region: MmioRegion, name: &'static str) -> DriverResult<NonNull<u8>> {
        devm_iomap(self, region, name)
    }

    fn devm_request_irq(
        &self,
        irq: IrqResource,
        handler: Arc<dyn DevIrqHandler>,
    ) -> DriverResult<()> {
        devm_request_irq(self, irq, handler)
    }

    fn devm_request_threaded_irq(
        &self,
        irq: IrqResource,
        primary: Arc<dyn DevIrqHandler>,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()> {
        devm_request_threaded_irq(self, irq, primary, thread, name)
    }

    fn devm_request_threaded_irq_default(
        &self,
        irq: IrqResource,
        thread: Arc<dyn DevIrqThreadHandler>,
        name: Option<&'static str>,
    ) -> DriverResult<()> {
        devm_request_threaded_irq_default(self, irq, thread, name)
    }

    fn devm_alloc_coherent(&self, spec: DmaSpec) -> DriverResult<(NonNull<u8>, u64)> {
        devm_alloc_coherent(self, spec)
    }
}
