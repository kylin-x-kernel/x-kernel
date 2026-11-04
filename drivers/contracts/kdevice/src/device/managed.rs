// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-managed (`devm_*`) helpers for explicit capability providers.
//!
//! Framework code passes provider objects explicitly to these helpers. This
//! crate does not own global provider state; provider selection belongs to
//! the host driver framework.
//!
//! Callers serialize acquisition with probe/removal. Successful acquisitions
//! register guards for framework-driven LIFO cleanup; failures register nothing.
//! Raw MMIO/DMA views are valid only until that cleanup, not until the last
//! device reference is dropped. The framework must quiesce hardware and users
//! first and run cleanup in the provider's permitted release context.

use alloc::sync::Arc;
use core::ptr::NonNull;

use driver_os_api::{
    CoherentMem, DevIrq, DmaOp, DmaSpec, IoMap, IrqHandler, IrqOp, IrqResource, IrqThreadHandler,
    MmioOp, MmioRegion, ResError, ResResult,
};

use crate::DeviceObject;

/// Map an MMIO region with an explicit provider and tie its lifetime to
/// `device`.
///
/// This is the preferred framework-facing form: provider selection is explicit
/// at the call boundary, while the cleanup remains device-managed.
pub fn devm_iomap_with_provider(
    provider: &'static dyn MmioOp,
    device: &DeviceObject,
    region: MmioRegion,
    name: &'static str,
) -> ResResult<NonNull<u8>> {
    let io = IoMap::map_with(provider, region, name)?;
    let ptr = NonNull::new(io.as_ptr()).ok_or(ResError::MappingFailed)?;
    device.add_cleanup(move || drop(io));
    Ok(ptr)
}

/// Register an interrupt handler with an explicit provider and tie its lifetime
/// to `device`.
pub fn devm_request_irq_with_provider(
    provider: &'static dyn IrqOp,
    device: &DeviceObject,
    irq: IrqResource,
    handler: Arc<dyn IrqHandler>,
) -> ResResult<()> {
    let guard = DevIrq::request_with(provider, irq, handler)?;
    device.add_cleanup(move || drop(guard));
    Ok(())
}

/// Register primary and threaded interrupt handlers with an explicit provider
/// and tie their lifetime to `device`.
pub fn devm_request_threaded_irq_with_provider(
    provider: &'static dyn IrqOp,
    device: &DeviceObject,
    irq: IrqResource,
    primary: Arc<dyn IrqHandler>,
    thread: Arc<dyn IrqThreadHandler>,
    name: Option<&'static str>,
) -> ResResult<()> {
    let guard = DevIrq::request_threaded_with(provider, irq, primary, thread, name)?;
    device.add_cleanup(move || drop(guard));
    Ok(())
}

/// Register a default-primary threaded interrupt handler with an explicit
/// provider and tie its lifetime to `device`.
pub fn devm_request_threaded_irq_default_with_provider(
    provider: &'static dyn IrqOp,
    device: &DeviceObject,
    irq: IrqResource,
    thread: Arc<dyn IrqThreadHandler>,
    name: Option<&'static str>,
) -> ResResult<()> {
    let guard = DevIrq::request_threaded_default_with(provider, irq, thread, name)?;
    device.add_cleanup(move || drop(guard));
    Ok(())
}

/// Allocate a coherent DMA buffer with an explicit provider and tie its
/// lifetime to `device`.
pub fn devm_alloc_coherent_with_provider(
    provider: &'static dyn DmaOp,
    device: &DeviceObject,
    spec: DmaSpec,
) -> ResResult<(NonNull<u8>, u64)> {
    let dma = CoherentMem::alloc_with(provider, spec)?;
    let cpu_ptr = NonNull::new(dma.as_ptr()).ok_or(ResError::InvalidResource)?;
    let bus_addr = dma.bus_addr();
    device.add_cleanup(move || drop(dma));
    Ok((cpu_ptr, bus_addr))
}

unittest::path_mod_test!(tests, "../../ut-tests/managed_resources.rs");
