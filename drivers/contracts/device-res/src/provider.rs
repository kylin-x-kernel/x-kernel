// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Device-managed (`devm_*`) helpers for explicit capability providers.
//!
//! Framework code passes provider objects explicitly to these helpers. This
//! crate does not own global provider state; provider selection belongs to
//! the host driver framework.

use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

use crate::{
    DeviceResource, ResResult,
    dma::{DmaCoherent, DmaOp, DmaSpec},
    irq::{Irq, IrqHandler, IrqOp, IrqResource, IrqThreadHandler},
    mmio::{Io, MmioOp, MmioRegion},
    time::TimeOp,
};

/// Complete capability provider for ordinary device drivers.
///
/// Host kernels may pass a concrete type implementing this trait into a driver
/// framework. The individual operation traits remain split so framework code
/// can require only the capability it is about to use.
pub trait ResourceProvider: MmioOp + DmaOp + IrqOp + TimeOp {}

impl<T: ?Sized> ResourceProvider for T where T: MmioOp + DmaOp + IrqOp + TimeOp {}

/// Map an MMIO region with an explicit provider and tie its lifetime to
/// `device`.
///
/// This is the preferred framework-facing form: provider selection is explicit
/// at the call boundary, while the cleanup remains device-managed.
pub fn devm_iomap_with_provider(
    provider: &'static dyn MmioOp,
    device: &dyn DeviceResource,
    region: MmioRegion,
    name: &'static str,
) -> ResResult<NonNull<u8>> {
    let io = Io::map_with(provider, region, name)?;
    let ptr = io.as_ptr();
    device.register_cleanup(Box::new(move || drop(io)));
    Ok(ptr)
}

/// Register an interrupt handler with an explicit provider and tie its lifetime
/// to `device`.
pub fn devm_request_irq_with_provider(
    provider: &'static dyn IrqOp,
    device: &dyn DeviceResource,
    irq: IrqResource,
    handler: Arc<dyn IrqHandler>,
) -> ResResult<()> {
    let guard = Irq::request_with(provider, irq, handler)?;
    device.register_cleanup(Box::new(move || drop(guard)));
    Ok(())
}

/// Register primary and threaded interrupt handlers with an explicit provider
/// and tie their lifetime to `device`.
pub fn devm_request_threaded_irq_with_provider(
    provider: &'static dyn IrqOp,
    device: &dyn DeviceResource,
    irq: IrqResource,
    primary: Arc<dyn IrqHandler>,
    thread: Arc<dyn IrqThreadHandler>,
    name: Option<&'static str>,
) -> ResResult<()> {
    let guard = Irq::request_threaded_with(provider, irq, primary, thread, name)?;
    device.register_cleanup(Box::new(move || drop(guard)));
    Ok(())
}

/// Register a default-primary threaded interrupt handler with an explicit
/// provider and tie its lifetime to `device`.
pub fn devm_request_threaded_irq_default_with_provider(
    provider: &'static dyn IrqOp,
    device: &dyn DeviceResource,
    irq: IrqResource,
    thread: Arc<dyn IrqThreadHandler>,
    name: Option<&'static str>,
) -> ResResult<()> {
    let guard = Irq::request_threaded_default_with(provider, irq, thread, name)?;
    device.register_cleanup(Box::new(move || drop(guard)));
    Ok(())
}

/// Allocate a coherent DMA buffer with an explicit provider and tie its
/// lifetime to `device`.
pub fn devm_alloc_coherent_with_provider(
    provider: &'static dyn DmaOp,
    device: &dyn DeviceResource,
    spec: DmaSpec,
) -> ResResult<(NonNull<u8>, u64)> {
    let dma = DmaCoherent::alloc_with(provider, spec)?;
    let cpu_ptr = dma.cpu_ptr();
    let bus_addr = dma.bus_addr();
    device.register_cleanup(Box::new(move || drop(dma)));
    Ok((cpu_ptr, bus_addr))
}
