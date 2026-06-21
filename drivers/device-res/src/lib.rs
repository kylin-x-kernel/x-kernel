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

#[cfg(unittest)]
mod unittest_tests {
    use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

    use unittest::def_test;

    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        MapMmio(MmioRegion),
        UnmapMmio(MmioRegion),
        RequestIrq(IrqResource),
        ReleaseIrq(IrqResource),
        SetIrqEnabled(IrqResource, bool),
        AllocCoherent(DmaSpec),
        FreeCoherent(DmaSpec),
    }

    struct ProviderState {
        events: Vec<Event>,
        mmio_words: [u64; 4],
        dma: [u8; 64],
        map_error: Option<ResError>,
        irq_error: Option<ResError>,
        dma_error: Option<ResError>,
    }

    impl ProviderState {
        const fn new() -> Self {
            Self {
                events: Vec::new(),
                mmio_words: [0; 4],
                dma: [0; 64],
                map_error: None,
                irq_error: None,
                dma_error: None,
            }
        }

        fn reset(&mut self) {
            self.events.clear();
            self.mmio_words.fill(0);
            self.dma.fill(0);
            self.map_error = None;
            self.irq_error = None;
            self.dma_error = None;
        }
    }

    struct TestProvider {
        state: SpinNoIrq<ProviderState>,
    }

    impl TestProvider {
        const fn new() -> Self {
            Self {
                state: SpinNoIrq::new(ProviderState::new()),
            }
        }

        fn reset(&self) {
            self.state.lock().reset();
        }

        fn set_map_error(&self, error: ResError) {
            self.state.lock().map_error = Some(error);
        }

        fn set_irq_error(&self, error: ResError) {
            self.state.lock().irq_error = Some(error);
        }

        fn set_dma_error(&self, error: ResError) {
            self.state.lock().dma_error = Some(error);
        }

        fn events(&self) -> Vec<Event> {
            self.state.lock().events.clone()
        }
    }

    impl ResourceProvider for TestProvider {
        fn map_mmio(&self, region: MmioRegion, _name: &'static str) -> ResResult<MmioMapping> {
            let mut state = self.state.lock();
            state.events.push(Event::MapMmio(region));
            if let Some(error) = state.map_error.take() {
                return Err(error);
            }
            Ok(MmioMapping {
                vaddr: state.mmio_words.as_mut_ptr() as usize,
                region,
            })
        }

        fn unmap_mmio(&self, mapping: MmioMapping) {
            self.state
                .lock()
                .events
                .push(Event::UnmapMmio(mapping.region));
        }

        fn request_irq(&self, irq: IrqResource, _handler: Arc<dyn IrqHandler>) -> ResResult<()> {
            let mut state = self.state.lock();
            state.events.push(Event::RequestIrq(irq));
            if let Some(error) = state.irq_error.take() {
                return Err(error);
            }
            Ok(())
        }

        fn release_irq(&self, irq: IrqResource) {
            self.state.lock().events.push(Event::ReleaseIrq(irq));
        }

        fn set_irq_enabled(&self, irq: IrqResource, enabled: bool) {
            self.state
                .lock()
                .events
                .push(Event::SetIrqEnabled(irq, enabled));
        }

        fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation> {
            let mut state = self.state.lock();
            state.events.push(Event::AllocCoherent(spec));
            if let Some(error) = state.dma_error.take() {
                return Err(error);
            }
            Ok(DmaAllocation {
                cpu_addr: state.dma.as_mut_ptr() as usize,
                bus_addr: 0xfeed_cafe,
                spec,
            })
        }

        fn free_coherent(&self, alloc: DmaAllocation) {
            self.state
                .lock()
                .events
                .push(Event::FreeCoherent(alloc.spec));
        }
    }

    struct TestDevice {
        resources: Vec<ResourceDesc>,
        cleanups: SpinNoIrq<Vec<Box<dyn FnOnce() + Send>>>,
    }

    impl TestDevice {
        fn new(resources: Vec<ResourceDesc>) -> Self {
            Self {
                resources,
                cleanups: SpinNoIrq::new(Vec::new()),
            }
        }

        fn run_cleanups(&self) {
            let mut cleanups = self.cleanups.lock();
            while let Some(clean) = cleanups.pop() {
                clean();
            }
        }
    }

    impl DeviceResource for TestDevice {
        fn resources(&self) -> &[ResourceDesc] {
            &self.resources
        }

        fn register_cleanup(&self, cleanup: Box<dyn FnOnce() + Send>) {
            self.cleanups.lock().push(cleanup);
        }
    }

    static TEST_PROVIDER: TestProvider = TestProvider::new();
    static TEST_SERIAL: SpinNoIrq<()> = SpinNoIrq::new(());

    fn install_test_provider() {
        TEST_PROVIDER.reset();
        set_provider(&TEST_PROVIDER);
    }

    #[def_test]
    fn provider_required_and_installation_visible() {
        let _serial = TEST_SERIAL.lock();
        *PROVIDER.lock() = None;
        assert_eq!(
            Io::map(
                MmioRegion {
                    base: 0x1000,
                    size: 4
                },
                "missing"
            )
            .unwrap_err(),
            ResError::NoProvider
        );

        install_test_provider();
        assert!(provider_installed());
    }

    #[def_test]
    fn io_read_write_and_drop_follow_provider_lifecycle() {
        let _serial = TEST_SERIAL.lock();
        install_test_provider();

        let region = MmioRegion {
            base: 0x2000,
            size: 16,
        };
        let io = Io::map(region, "regs").unwrap();
        assert_eq!(io.region(), region);

        io.write8(0, 0x12);
        io.write16(2, 0x3456);
        io.write32(4, 0x789a_bcde);
        io.write64(8, 0x0123_4567_89ab_cdef);

        assert_eq!(io.read8(0), 0x12);
        assert_eq!(io.read16(2), 0x3456);
        assert_eq!(io.read32(4), 0x789a_bcde);
        assert_eq!(io.read64(8), 0x0123_4567_89ab_cdef);

        drop(io);

        assert_eq!(
            TEST_PROVIDER.events(),
            vec![Event::MapMmio(region), Event::UnmapMmio(region)]
        );
    }

    #[def_test]
    fn irq_and_dma_release_on_drop_and_devm_cleanup() {
        let _serial = TEST_SERIAL.lock();
        install_test_provider();

        let irq = IrqResource {
            number: 7,
            trigger: IrqTriggerMode::LevelHigh,
        };
        let spec = DmaSpec { len: 24, align: 8 };

        let guard = Irq::request(irq, Arc::new(|| IrqReturn::Handled)).unwrap();
        assert_eq!(guard.number(), 7);
        guard.set_enabled(false);
        drop(guard);

        let dma = DmaCoherent::alloc(spec).unwrap();
        assert_eq!(dma.bus_addr(), 0xfeed_cafe);
        assert_eq!(dma.len(), 24);
        assert!(!dma.is_empty());
        drop(dma);

        let device = TestDevice::new(vec![ResourceDesc::Irq(irq), ResourceDesc::Dma(spec)]);
        assert_eq!(device.resources().len(), 2);

        devm_request_irq(&device, irq, Arc::new(|| IrqReturn::Handled)).unwrap();
        let (cpu_ptr, bus_addr) = devm_alloc_coherent(&device, spec).unwrap();
        assert_eq!(
            cpu_ptr.as_ptr() as usize,
            TEST_PROVIDER.state.lock().dma.as_ptr() as usize
        );
        assert_eq!(bus_addr, 0xfeed_cafe);

        device.run_cleanups();

        assert_eq!(
            TEST_PROVIDER.events(),
            vec![
                Event::RequestIrq(irq),
                Event::SetIrqEnabled(irq, false),
                Event::ReleaseIrq(irq),
                Event::AllocCoherent(spec),
                Event::FreeCoherent(spec),
                Event::RequestIrq(irq),
                Event::AllocCoherent(spec),
                Event::FreeCoherent(spec),
                Event::ReleaseIrq(irq),
            ]
        );
    }

    #[def_test]
    fn devm_iomap_and_provider_errors_propagate() {
        let _serial = TEST_SERIAL.lock();
        install_test_provider();

        let device = TestDevice::new(Vec::new());
        let region = MmioRegion {
            base: 0x3000,
            size: 8,
        };
        let ptr = devm_iomap(&device, region, "devm").unwrap();
        assert_eq!(
            ptr.as_ptr() as usize,
            TEST_PROVIDER.state.lock().mmio_words.as_ptr() as usize
        );
        device.run_cleanups();

        TEST_PROVIDER.reset();
        TEST_PROVIDER.set_map_error(ResError::MappingFailed);
        assert_eq!(
            devm_iomap(&device, region, "failing").unwrap_err(),
            ResError::MappingFailed
        );

        TEST_PROVIDER.reset();
        TEST_PROVIDER.set_irq_error(ResError::Busy);
        assert_eq!(
            devm_request_irq(
                &device,
                IrqResource {
                    number: 9,
                    trigger: IrqTriggerMode::EdgeRising,
                },
                Arc::new(|| IrqReturn::NotHandled),
            )
            .unwrap_err(),
            ResError::Busy
        );

        TEST_PROVIDER.reset();
        TEST_PROVIDER.set_dma_error(ResError::NoMemory);
        assert_eq!(
            devm_alloc_coherent(&device, DmaSpec { len: 8, align: 8 }).unwrap_err(),
            ResError::NoMemory
        );
    }
}
