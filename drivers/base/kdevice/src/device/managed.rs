// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-managed (`devm_*`) helpers for explicit capability providers.
//!
//! Framework code passes provider objects explicitly to these helpers. This
//! crate does not own global provider state; provider selection belongs to
//! the host driver framework.
//!
//! Cleanup belongs to one **binding instance**, not to the stable device
//! object: callers pass the [`BindingRef`] they captured in the lifecycle write
//! context together with the stable object. The helper verifies that the
//! captured runtime is still the object's current binding before registering,
//! so a stale handle from a previous binding can never attach cleanup to its
//! replacement.
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

use crate::{BindingRef, DeviceObject};

/// Register a cleanup on the captured binding instance.
fn add_binding_cleanup<F>(binding: &BindingRef, device: &DeviceObject, cleanup: F) -> ResResult<()>
where
    F: FnOnce() + Send + 'static,
{
    binding
        .add_cleanup(device, cleanup)
        .map_err(|_| ResError::BadState)
}

/// Map an MMIO region with an explicit provider and tie its lifetime to the
/// captured binding of `device`.
///
/// This is the preferred framework-facing form: provider selection is explicit
/// at the call boundary, while the cleanup remains device-managed and is owned
/// by exactly the binding that acquired the mapping.
pub fn devm_iomap_with_provider(
    provider: &'static dyn MmioOp,
    binding: &BindingRef,
    device: &DeviceObject,
    region: MmioRegion,
    name: &'static str,
) -> ResResult<NonNull<u8>> {
    let io = IoMap::map_with(provider, region, name)?;
    let ptr = NonNull::new(io.as_ptr()).ok_or(ResError::MappingFailed)?;
    add_binding_cleanup(binding, device, move || drop(io))?;
    Ok(ptr)
}

/// Register an interrupt handler with an explicit provider and tie its
/// lifetime to the captured binding of `device`.
pub fn devm_request_irq_with_provider(
    provider: &'static dyn IrqOp,
    binding: &BindingRef,
    device: &DeviceObject,
    irq: IrqResource,
    handler: Arc<dyn IrqHandler>,
) -> ResResult<()> {
    let guard = DevIrq::request_with(provider, irq, handler)?;
    add_binding_cleanup(binding, device, move || drop(guard))
}

/// Register primary and threaded interrupt handlers with an explicit provider
/// and tie their lifetime to the captured binding of `device`.
pub fn devm_request_threaded_irq_with_provider(
    provider: &'static dyn IrqOp,
    binding: &BindingRef,
    device: &DeviceObject,
    irq: IrqResource,
    primary: Arc<dyn IrqHandler>,
    thread: Arc<dyn IrqThreadHandler>,
    name: Option<&'static str>,
) -> ResResult<()> {
    let guard = DevIrq::request_threaded_with(provider, irq, primary, thread, name)?;
    add_binding_cleanup(binding, device, move || drop(guard))
}

/// Register a default-primary threaded interrupt handler with an explicit
/// provider and tie its lifetime to the captured binding of `device`.
pub fn devm_request_threaded_irq_default_with_provider(
    provider: &'static dyn IrqOp,
    binding: &BindingRef,
    device: &DeviceObject,
    irq: IrqResource,
    thread: Arc<dyn IrqThreadHandler>,
    name: Option<&'static str>,
) -> ResResult<()> {
    let guard = DevIrq::request_threaded_default_with(provider, irq, thread, name)?;
    add_binding_cleanup(binding, device, move || drop(guard))
}

/// Allocate a coherent DMA buffer with an explicit provider and tie its
/// lifetime to the captured binding of `device`.
pub fn devm_alloc_coherent_with_provider(
    provider: &'static dyn DmaOp,
    binding: &BindingRef,
    device: &DeviceObject,
    spec: DmaSpec,
) -> ResResult<(NonNull<u8>, u64)> {
    let dma = CoherentMem::alloc_with(provider, spec)?;
    let cpu_ptr = NonNull::new(dma.as_ptr()).ok_or(ResError::InvalidResource)?;
    let bus_addr = dma.bus_addr();
    add_binding_cleanup(binding, device, move || drop(dma))?;
    Ok((cpu_ptr, bus_addr))
}

unittest::path_mod_test!(tests, "../../ut-tests/managed_resources.rs");
