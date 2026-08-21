// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Global capability provider registry and device-managed (`devm_*`) helpers.
//!
//! The host kernel installs one [`MmioOp`], [`DmaOp`], and [`IrqOp`]
//! implementation each during early init. Drivers acquire resources either
//! through the RAII handles ([`Io`], [`Irq`], [`DmaCoherent`]) or the `devm_*`
//! helpers that bind resource lifetime to a [`DeviceResource`].

use alloc::{boxed::Box, sync::Arc};
use core::ptr::NonNull;

#[cfg(not(unittest))]
use klazy::Once;

use crate::{
    DeviceResource, ResError, ResResult,
    dma::{DmaCoherent, DmaOp, DmaSpec},
    irq::{Irq, IrqHandler, IrqOp, IrqResource},
    mmio::{Io, MmioOp, MmioRegion},
};

// Capability provider registry.
//
// Production uses `klazy::Once` for lock-free reads (a single acquire load
// after installation). Under `--cfg unittest` the registry remains install-once
// so full-kernel parallel tests cannot replace providers while real device
// background tasks are still using them.
#[cfg(not(unittest))]
static MMIO_PROVIDER: Once<&'static dyn MmioOp> = Once::new();
#[cfg(not(unittest))]
static DMA_PROVIDER: Once<&'static dyn DmaOp> = Once::new();
#[cfg(not(unittest))]
static IRQ_PROVIDER: Once<&'static dyn IrqOp> = Once::new();

#[cfg(unittest)]
static MMIO_PROVIDER: kspin::SpinNoIrq<Option<&'static dyn MmioOp>> = kspin::SpinNoIrq::new(None);
#[cfg(unittest)]
static DMA_PROVIDER: kspin::SpinNoIrq<Option<&'static dyn DmaOp>> = kspin::SpinNoIrq::new(None);
#[cfg(unittest)]
static IRQ_PROVIDER: kspin::SpinNoIrq<Option<&'static dyn IrqOp>> = kspin::SpinNoIrq::new(None);

/// Install the MMIO capability provider. Must be called exactly once during
/// early kernel init, before any driver acquires a resource.
#[cfg(not(unittest))]
pub fn set_mmio_provider(p: &'static dyn MmioOp) {
    MMIO_PROVIDER.call_once(|| p);
}
#[cfg(not(unittest))]
pub fn set_dma_provider(p: &'static dyn DmaOp) {
    DMA_PROVIDER.call_once(|| p);
}
#[cfg(not(unittest))]
pub fn set_irq_provider(p: &'static dyn IrqOp) {
    IRQ_PROVIDER.call_once(|| p);
}

#[cfg(unittest)]
pub fn set_mmio_provider(p: &'static dyn MmioOp) {
    let mut provider = MMIO_PROVIDER.lock();
    if provider.is_none() {
        *provider = Some(p);
    }
}
#[cfg(unittest)]
pub fn set_dma_provider(p: &'static dyn DmaOp) {
    let mut provider = DMA_PROVIDER.lock();
    if provider.is_none() {
        *provider = Some(p);
    }
}
#[cfg(unittest)]
pub fn set_irq_provider(p: &'static dyn IrqOp) {
    let mut provider = IRQ_PROVIDER.lock();
    if provider.is_none() {
        *provider = Some(p);
    }
}

/// Returns `true` once all three providers have been installed.
pub fn provider_installed() -> bool {
    try_mmio_provider().is_some() && try_dma_provider().is_some() && try_irq_provider().is_some()
}

/// Returns the installed MMIO provider, or `None` if unset.
#[cfg(not(unittest))]
pub fn try_mmio_provider() -> Option<&'static dyn MmioOp> {
    MMIO_PROVIDER.get().copied()
}
#[cfg(unittest)]
pub fn try_mmio_provider() -> Option<&'static dyn MmioOp> {
    *MMIO_PROVIDER.lock()
}

/// Returns the installed DMA provider, or `None` if unset.
#[cfg(not(unittest))]
pub fn try_dma_provider() -> Option<&'static dyn DmaOp> {
    DMA_PROVIDER.get().copied()
}
#[cfg(unittest)]
pub fn try_dma_provider() -> Option<&'static dyn DmaOp> {
    *DMA_PROVIDER.lock()
}

/// Returns the installed IRQ provider, or `None` if unset.
#[cfg(not(unittest))]
pub fn try_irq_provider() -> Option<&'static dyn IrqOp> {
    IRQ_PROVIDER.get().copied()
}
#[cfg(unittest)]
pub fn try_irq_provider() -> Option<&'static dyn IrqOp> {
    *IRQ_PROVIDER.lock()
}

/// Returns the installed MMIO provider, or [`NoProvider`](ResError::NoProvider).
pub fn mmio_provider() -> ResResult<&'static dyn MmioOp> {
    try_mmio_provider().ok_or(ResError::NoProvider)
}
/// Returns the installed DMA provider, or [`NoProvider`](ResError::NoProvider).
pub fn dma_provider() -> ResResult<&'static dyn DmaOp> {
    try_dma_provider().ok_or(ResError::NoProvider)
}
/// Returns the installed IRQ provider, or [`NoProvider`](ResError::NoProvider).
pub fn irq_provider() -> ResResult<&'static dyn IrqOp> {
    try_irq_provider().ok_or(ResError::NoProvider)
}

/// Map an MMIO region and tie its lifetime to `device`.
///
/// The mapping is released when the device's probe fails or it is removed.
/// Returns the virtual base pointer directly (an [`Io`] handle would outlive
/// the function scope). Use [`Io::map`] when manual lifetime control is needed.
///
/// # Errors
///
/// See [`Io::map`].
pub fn devm_iomap(
    device: &dyn DeviceResource,
    region: MmioRegion,
    name: &'static str,
) -> ResResult<NonNull<u8>> {
    let io = Io::map(region, name)?;
    let ptr = io.as_ptr();
    device.register_cleanup(Box::new(move || drop(io)));
    Ok(ptr)
}

/// Register an interrupt handler and tie its lifetime to `device`.
///
/// # Errors
///
/// See [`Irq::request`].
pub fn devm_request_irq(
    device: &dyn DeviceResource,
    irq: IrqResource,
    handler: Arc<dyn IrqHandler>,
) -> ResResult<()> {
    let guard = Irq::request(irq, handler)?;
    device.register_cleanup(Box::new(move || drop(guard)));
    Ok(())
}

/// Allocate a coherent DMA buffer and tie its lifetime to `device`.
///
/// Returns the CPU virtual address and device bus address.
///
/// # Errors
///
/// See [`DmaCoherent::alloc`].
pub fn devm_alloc_coherent(
    device: &dyn DeviceResource,
    spec: DmaSpec,
) -> ResResult<(NonNull<u8>, u64)> {
    let dma = DmaCoherent::alloc(spec)?;
    let cpu_ptr = dma.cpu_ptr();
    let bus_addr = dma.bus_addr();
    device.register_cleanup(Box::new(move || drop(dma)));
    Ok((cpu_ptr, bus_addr))
}
