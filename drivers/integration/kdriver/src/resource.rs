// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Driver-facing resource helpers backed by the [`device_res`] provider model.
//!
//! The X-Kernel provider implementation lives in `device-res-xkernel`. This
//! module keeps the driver orchestration crate's `DriverResult` boundary while
//! acquiring resources through the OS-neutral `device_res` API.

use alloc::sync::Arc;
use core::ptr::NonNull;

use device_res::{
    DmaSpec, IrqHandler as DevIrqHandler, IrqResource, IrqThreadHandler as DevIrqThreadHandler,
    MmioRegion, ResError,
};
use device_res_xkernel::XKernelResourceProvider;
use driver_base::{DriverError, DriverResult};
use kdevice::DeviceObject;

static XKERNEL_RESOURCE_PROVIDER: XKernelResourceProvider = XKernelResourceProvider::new();

pub(crate) fn resource_provider() -> &'static XKernelResourceProvider {
    &XKERNEL_RESOURCE_PROVIDER
}

fn map_res_err(err: ResError) -> DriverError {
    match err {
        ResError::InvalidResource => DriverError::InvalidInput,
        ResError::MappingFailed => DriverError::Io,
        ResError::NoMemory => DriverError::NoMemory,
        ResError::Busy => DriverError::ResourceBusy,
        ResError::Unsupported => DriverError::Unsupported,
    }
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
    device_res::devm_iomap_with_provider(resource_provider(), device, region, name)
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
    device_res::devm_request_irq_with_provider(resource_provider(), device, irq, handler)
        .map_err(map_res_err)
}

/// Register primary and threaded interrupt handlers and tie their lifetime to
/// `device`.
///
/// The request is routed through the framework-owned `device_res` IRQ provider.
pub(crate) fn devm_request_threaded_irq(
    device: &DeviceObject,
    irq: IrqResource,
    primary: Arc<dyn DevIrqHandler>,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    device_res::devm_request_threaded_irq_with_provider(
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
/// The request is routed through the framework-owned `device_res` IRQ provider.
pub(crate) fn devm_request_threaded_irq_default(
    device: &DeviceObject,
    irq: IrqResource,
    thread: Arc<dyn DevIrqThreadHandler>,
    name: Option<&'static str>,
) -> DriverResult<()> {
    device_res::devm_request_threaded_irq_default_with_provider(
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
    device_res::devm_alloc_coherent_with_provider(resource_provider(), device, spec)
        .map_err(map_res_err)
}

/// Driver-facing device-managed resource methods.
///
/// Drivers use this extension trait to acquire resources from their
/// [`DeviceObject`] without naming the concrete host provider. The provider is
/// owned by the driver framework and passed explicitly into `device_res`.
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
