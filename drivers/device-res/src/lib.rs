// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS-agnostic device resource model and provider abstraction.
//!
//! This crate describes the hardware resources a driver consumes (MMIO regions,
//! I/O port ranges, interrupts, and coherent DMA buffers) without binding to any
//! particular kernel. A host kernel installs a [`ResourceProvider`] backend once
//! during early init; drivers then acquire resources through RAII handles
//! ([`Io`], [`Irq`], [`DmaCoherent`]) that release their backing resource on
//! drop.
//!
//! Keeping the OS-semantic operations (map/unmap, request/release IRQ, allocate
//! /free coherent memory) behind a single trait isolates driver code from the
//! host kernel: porting a driver to another kernel only requires implementing
//! the provider, not rewriting the driver.
#![no_std]

extern crate alloc;

use alloc::{boxed::Box, sync::Arc};
use core::{
    ptr::NonNull,
    sync::atomic::{Ordering, fence},
};

use kspin::SpinNoIrq;

/// A memory-mapped I/O region described by physical address and size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MmioRegion {
    /// Physical base address.
    pub base: usize,
    /// Region size in bytes.
    pub size: usize,
}

/// An x86 I/O port range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IoPortRange {
    /// Base port number.
    pub base: u16,
    /// Number of consecutive ports.
    pub size: u16,
}

/// Interrupt trigger mode.
///
/// This is intentionally OS-neutral. Host kernels convert their own trigger
/// representation into this enum at discovery time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqTriggerMode {
    /// Rising-edge triggered.
    EdgeRising,
    /// Falling-edge triggered.
    EdgeFalling,
    /// Active-high level triggered.
    LevelHigh,
    /// Active-low level triggered.
    LevelLow,
    /// Trigger mode not described by firmware.
    Unspecified,
}

/// An interrupt resource.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct IrqResource {
    /// IRQ number (GSI or platform-specific).
    pub number: usize,
    /// Trigger mode.
    pub trigger: IrqTriggerMode,
}

/// A request for a coherent DMA buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DmaSpec {
    /// Buffer length in bytes.
    pub len: usize,
    /// Required alignment in bytes (power of two).
    pub align: usize,
}

/// A single resource associated with a device.
#[derive(Debug, Clone, Copy)]
pub enum ResourceDesc {
    /// Memory-mapped I/O region.
    Mmio(MmioRegion),
    /// x86 I/O port range.
    IoPort(IoPortRange),
    /// Interrupt line.
    Irq(IrqResource),
    /// Coherent DMA buffer request.
    Dma(DmaSpec),
}

/// A collection of resources for a single device.
pub type ResourceSet = smallvec::SmallVec<[ResourceDesc; 4]>;

/// Error returned by resource acquisition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResError {
    /// The resource descriptor was invalid (e.g. zero-sized region).
    InvalidResource,
    /// The host failed to establish the requested mapping.
    MappingFailed,
    /// The host ran out of memory while acquiring the resource.
    NoMemory,
    /// The resource is already in use and cannot be shared.
    Busy,
    /// The host does not support this kind of resource.
    Unsupported,
    /// No resource provider has been installed yet.
    NoProvider,
}

/// Result type for resource operations.
pub type ResResult<T = ()> = Result<T, ResError>;

/// Outcome reported by an interrupt handler.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IrqReturn {
    /// The interrupt was claimed and serviced by this handler.
    Handled,
    /// The interrupt was not for this handler (e.g. a shared line).
    NotHandled,
}

/// A device interrupt handler.
///
/// Handlers run in interrupt context: they must not block, must not allocate,
/// and should defer heavy work to a thread. Any closure that is
/// `Fn() -> IrqReturn + Send + Sync` implements this trait, so a driver can
/// capture its own state in the closure rather than reaching for global state.
pub trait IrqHandler: Send + Sync {
    /// Service a fired interrupt.
    fn handle(&self) -> IrqReturn;
}

impl<F> IrqHandler for F
where
    F: Fn() -> IrqReturn + Send + Sync,
{
    fn handle(&self) -> IrqReturn {
        self()
    }
}

/// A mapping token returned by [`ResourceProvider::map_mmio`].
///
/// The provider is responsible for interpreting the token in
/// [`ResourceProvider::unmap_mmio`]. Drivers never construct this directly; they
/// hold an [`Io`] handle instead.
#[derive(Debug, Clone, Copy)]
pub struct MmioMapping {
    /// Virtual address the CPU uses to access the region.
    pub vaddr: usize,
    /// The physical region this mapping covers.
    pub region: MmioRegion,
}

/// A coherent DMA allocation returned by [`ResourceProvider::alloc_coherent`].
#[derive(Debug, Clone, Copy)]
pub struct DmaAllocation {
    /// Virtual address the CPU uses to access the buffer.
    pub cpu_addr: usize,
    /// Bus address the device uses to access the buffer.
    pub bus_addr: u64,
    /// The originating allocation request.
    pub spec: DmaSpec,
}

/// Host-provided backend for OS-semantic resource operations.
///
/// A host kernel implements this trait and installs it once via
/// [`set_provider`]. All methods may run in normal (non-interrupt) context
/// during device probe and removal.
pub trait ResourceProvider: Sync {
    /// Map an MMIO region and return a token for later teardown.
    fn map_mmio(&self, region: MmioRegion, name: &'static str) -> ResResult<MmioMapping>;

    /// Release a mapping previously returned by [`Self::map_mmio`].
    fn unmap_mmio(&self, mapping: MmioMapping);

    /// Register an interrupt handler for `irq`.
    fn request_irq(&self, irq: IrqResource, handler: Arc<dyn IrqHandler>) -> ResResult<()>;

    /// Release an interrupt handler previously registered for `irq`.
    fn release_irq(&self, irq: IrqResource);

    /// Enable or disable delivery of `irq`.
    fn set_irq_enabled(&self, irq: IrqResource, enabled: bool);

    /// Allocate a coherent DMA buffer.
    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation>;

    /// Free a coherent DMA buffer previously returned by [`Self::alloc_coherent`].
    fn free_coherent(&self, alloc: DmaAllocation);
}

static PROVIDER: SpinNoIrq<Option<&'static dyn ResourceProvider>> = SpinNoIrq::new(None);

/// Install the host resource provider.
///
/// This is expected to be called exactly once during early kernel init, before
/// any driver acquires a resource. Subsequent calls replace the provider.
pub fn set_provider(provider: &'static dyn ResourceProvider) {
    *PROVIDER.lock() = Some(provider);
}

/// Returns `true` once a provider has been installed.
pub fn provider_installed() -> bool {
    PROVIDER.lock().is_some()
}

fn try_provider() -> Option<&'static dyn ResourceProvider> {
    *PROVIDER.lock()
}

fn provider() -> ResResult<&'static dyn ResourceProvider> {
    try_provider().ok_or(ResError::NoProvider)
}

/// RAII handle to a mapped MMIO region.
///
/// Dropping the handle releases the mapping through the installed provider.
#[derive(Debug)]
pub struct Io {
    mapping: Option<MmioMapping>,
}

impl Io {
    /// Map an MMIO region, returning a handle that unmaps on drop.
    ///
    /// # Errors
    ///
    /// Returns [`NoProvider`](ResError::NoProvider) if no provider has been
    /// installed. May also return errors propagated from the provider
    /// (e.g. [`MappingFailed`](ResError::MappingFailed),
    /// [`InvalidResource`](ResError::InvalidResource)).
    pub fn map(region: MmioRegion, name: &'static str) -> ResResult<Self> {
        let mapping = provider()?.map_mmio(region, name)?;
        Ok(Self {
            mapping: Some(mapping),
        })
    }

    /// The virtual base address of the mapping.
    ///
    /// # Panics
    ///
    /// Panics if the handle has no active mapping. This cannot happen through
    /// the current public API since the mapping is only taken during drop.
    pub fn as_ptr(&self) -> NonNull<u8> {
        NonNull::new(
            self.mapping
                .as_ref()
                .expect("Io handle used after release")
                .vaddr as *mut u8,
        )
        .expect("Io mapping stored a null virtual address")
    }

    /// The physical region backing this mapping.
    ///
    /// # Panics
    ///
    /// Panics if the handle has no active mapping. This cannot happen through
    /// the current public API since the mapping is only taken during drop.
    pub fn region(&self) -> MmioRegion {
        self.mapping
            .as_ref()
            .expect("Io handle used after release")
            .region
    }

    /// Returns a checked pointer `offset` bytes into the region, asserting that
    /// `[offset, offset + size)` stays within bounds.
    ///
    /// # Panics
    ///
    /// Panics if `offset + size` overflows `usize` or exceeds `region.size`.
    #[inline]
    fn access_ptr(&self, offset: usize, size: usize) -> *mut u8 {
        let region = self.region();
        let end = offset
            .checked_add(size)
            .expect("MMIO access offset overflow");
        assert!(end <= region.size, "MMIO access out of bounds");
        // SAFETY: the offset has been bounds-checked against the mapped region
        // length, so the resulting pointer stays inside the mapping.
        unsafe { self.as_ptr().as_ptr().add(offset) }
    }
}

/// Architecture-neutral memory-mapped register accessors.
///
/// Reads use an acquire fence and writes a release fence so that register
/// accesses are not reordered across the access by the compiler or the CPU.
/// On strongly-ordered targets (e.g. x86) the fences compile to no-ops; on
/// weakly-ordered targets (e.g. AArch64) they emit the appropriate barrier.
/// Offsets are bounds-checked against the mapped region and must be naturally
/// aligned for the access width.
///
/// # Panics
///
/// All accessors panic if `offset + width` overflows `usize` or exceeds the
/// mapped region size. Multi-byte accessors panic on misaligned offsets in
/// debug builds.
impl Io {
    /// Read a `u8` register at `offset`.
    #[inline]
    pub fn read8(&self, offset: usize) -> u8 {
        let ptr = self.access_ptr(offset, 1);
        // SAFETY: `access_ptr` bounds-checked the access; `u8` has no alignment
        // requirement.
        let value = unsafe { ptr.read_volatile() };
        fence(Ordering::Acquire);
        value
    }

    /// Read a `u16` register at `offset` (must be 2-byte aligned).
    #[inline]
    pub fn read16(&self, offset: usize) -> u16 {
        let ptr = self.access_ptr(offset, 2) as *const u16;
        debug_assert!((ptr as usize).is_multiple_of(2), "unaligned MMIO u16 read");
        // SAFETY: bounds- and alignment-checked above.
        let value = unsafe { ptr.read_volatile() };
        fence(Ordering::Acquire);
        value
    }

    /// Read a `u32` register at `offset` (must be 4-byte aligned).
    #[inline]
    pub fn read32(&self, offset: usize) -> u32 {
        let ptr = self.access_ptr(offset, 4) as *const u32;
        debug_assert!((ptr as usize).is_multiple_of(4), "unaligned MMIO u32 read");
        // SAFETY: bounds- and alignment-checked above.
        let value = unsafe { ptr.read_volatile() };
        fence(Ordering::Acquire);
        value
    }

    /// Read a `u64` register at `offset` (must be 8-byte aligned).
    #[inline]
    pub fn read64(&self, offset: usize) -> u64 {
        let ptr = self.access_ptr(offset, 8) as *const u64;
        debug_assert!((ptr as usize).is_multiple_of(8), "unaligned MMIO u64 read");
        // SAFETY: bounds- and alignment-checked above.
        let value = unsafe { ptr.read_volatile() };
        fence(Ordering::Acquire);
        value
    }

    /// Write a `u8` register at `offset`.
    #[inline]
    pub fn write8(&self, offset: usize, value: u8) {
        let ptr = self.access_ptr(offset, 1);
        fence(Ordering::Release);
        // SAFETY: `access_ptr` bounds-checked the access; `u8` has no alignment
        // requirement.
        unsafe { ptr.write_volatile(value) };
    }

    /// Write a `u16` register at `offset` (must be 2-byte aligned).
    #[inline]
    pub fn write16(&self, offset: usize, value: u16) {
        let ptr = self.access_ptr(offset, 2) as *mut u16;
        debug_assert!((ptr as usize).is_multiple_of(2), "unaligned MMIO u16 write");
        fence(Ordering::Release);
        // SAFETY: bounds- and alignment-checked above.
        unsafe { ptr.write_volatile(value) };
    }

    /// Write a `u32` register at `offset` (must be 4-byte aligned).
    #[inline]
    pub fn write32(&self, offset: usize, value: u32) {
        let ptr = self.access_ptr(offset, 4) as *mut u32;
        debug_assert!((ptr as usize).is_multiple_of(4), "unaligned MMIO u32 write");
        fence(Ordering::Release);
        // SAFETY: bounds- and alignment-checked above.
        unsafe { ptr.write_volatile(value) };
    }

    /// Write a `u64` register at `offset` (must be 8-byte aligned).
    #[inline]
    pub fn write64(&self, offset: usize, value: u64) {
        let ptr = self.access_ptr(offset, 8) as *mut u64;
        debug_assert!((ptr as usize).is_multiple_of(8), "unaligned MMIO u64 write");
        fence(Ordering::Release);
        // SAFETY: bounds- and alignment-checked above.
        unsafe { ptr.write_volatile(value) };
    }
}

impl Drop for Io {
    fn drop(&mut self) {
        if let (Some(mapping), Some(provider)) = (self.mapping.take(), try_provider()) {
            provider.unmap_mmio(mapping);
        }
    }
}

/// RAII handle to a registered interrupt.
///
/// Dropping the handle releases the handler through the installed provider.
#[derive(Debug)]
pub struct Irq {
    resource: IrqResource,
    armed: bool,
}

impl Irq {
    /// Register `handler` for the interrupt described by `resource`.
    ///
    /// # Errors
    ///
    /// Returns [`NoProvider`](ResError::NoProvider) if no provider has been
    /// installed. May also return errors propagated from the provider
    /// (e.g. [`Busy`](ResError::Busy),
    /// [`InvalidResource`](ResError::InvalidResource)).
    pub fn request(resource: IrqResource, handler: Arc<dyn IrqHandler>) -> ResResult<Self> {
        provider()?.request_irq(resource, handler)?;
        Ok(Self {
            resource,
            armed: true,
        })
    }

    /// Enable or disable delivery of this interrupt.
    pub fn set_enabled(&self, enabled: bool) {
        if let Some(provider) = try_provider() {
            provider.set_irq_enabled(self.resource, enabled);
        }
    }

    /// The interrupt number.
    pub fn number(&self) -> usize {
        self.resource.number
    }
}

impl Drop for Irq {
    fn drop(&mut self) {
        if self.armed
            && let Some(provider) = try_provider()
        {
            provider.release_irq(self.resource);
        }
    }
}

/// RAII handle to a coherent DMA buffer.
///
/// Dropping the handle frees the buffer through the installed provider.
#[derive(Debug)]
pub struct DmaCoherent {
    allocation: Option<DmaAllocation>,
}

impl DmaCoherent {
    /// Allocate a coherent DMA buffer, returning a handle that frees on drop.
    ///
    /// # Errors
    ///
    /// Returns [`NoProvider`](ResError::NoProvider) if no provider has been
    /// installed. May also return errors propagated from the provider
    /// (e.g. [`NoMemory`](ResError::NoMemory),
    /// [`Unsupported`](ResError::Unsupported)).
    pub fn alloc(spec: DmaSpec) -> ResResult<Self> {
        let allocation = provider()?.alloc_coherent(spec)?;
        Ok(Self {
            allocation: Some(allocation),
        })
    }

    /// The CPU-visible virtual address of the buffer.
    ///
    /// # Panics
    ///
    /// Panics if the handle has no active allocation. This cannot happen through
    /// the current public API since the allocation is only taken during drop.
    pub fn cpu_ptr(&self) -> NonNull<u8> {
        NonNull::new(
            self.allocation
                .as_ref()
                .expect("DmaCoherent handle used after release")
                .cpu_addr as *mut u8,
        )
        .expect("DmaCoherent stored a null CPU address")
    }

    /// The device-visible bus address of the buffer.
    ///
    /// # Panics
    ///
    /// Panics if the handle has no active allocation. This cannot happen through
    /// the current public API since the allocation is only taken during drop.
    pub fn bus_addr(&self) -> u64 {
        self.allocation
            .as_ref()
            .expect("DmaCoherent handle used after release")
            .bus_addr
    }

    /// The buffer length in bytes.
    ///
    /// # Panics
    ///
    /// Panics if the handle has no active allocation. This cannot happen through
    /// the current public API since the allocation is only taken during drop.
    pub fn len(&self) -> usize {
        self.allocation
            .as_ref()
            .expect("DmaCoherent handle used after release")
            .spec
            .len
    }

    /// Returns `true` if the buffer has zero length.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Drop for DmaCoherent {
    fn drop(&mut self) {
        if let (Some(allocation), Some(provider)) = (self.allocation.take(), try_provider()) {
            provider.free_coherent(allocation);
        }
    }
}

/// OS-agnostic view of a device that a driver binds resources to.
///
/// A host kernel implements this for its concrete device object so that the
/// device-managed (`devm_*`) helpers and driver code can read the discovered
/// resources and attach resource teardown without depending on a kernel type.
/// Drivers can therefore be written against `&dyn DeviceResource` and ported by
/// reimplementing only the host adapter and the [`ResourceProvider`].
pub trait DeviceResource {
    /// The hardware resources discovered for this device.
    fn resources(&self) -> &[ResourceDesc];

    /// Register a teardown callback bound to the device's lifetime.
    ///
    /// Callbacks run in reverse (LIFO) order when the device's probe fails or
    /// when the device is removed.
    fn register_cleanup(&self, cleanup: Box<dyn FnOnce() + Send>);
}

/// Map an MMIO region and tie its lifetime to `device`.
///
/// The mapping is released when the device's probe fails or it is removed.
/// Since returning an [`Io`] handle that borrows the device's lifetime is not
/// possible (the handle would outlive the function scope), this returns the
/// virtual base pointer directly. Use [`Io::map`] when manual lifetime control
/// is required.
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
/// The handler is released when the device's probe fails or it is removed.
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
/// The buffer is freed when the device's probe fails or it is removed. Returns
/// the CPU virtual address and device bus address of the buffer.
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
