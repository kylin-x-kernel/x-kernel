// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS-agnostic device resource model and capability abstraction.
//!
//! This crate describes the hardware resources a driver consumes (MMIO
//! regions, I/O port ranges, interrupts, coherent/streaming DMA buffers)
//! without binding to any particular kernel. A host kernel installs three
//! capability providers — [`MmioOp`], [`DmaOp`], [`IrqOp`] — once during early
//! init; drivers then acquire resources through RAII handles ([`Io`], [`Irq`],
//! [`DmaCoherent`]) or the device-managed `devm_*` helpers.
//!
//! Keeping the OS-semantic operations (map/unmap, request/release IRQ,
//! allocate/free DMA) behind per-resource-type traits isolates driver code
//! from the host kernel: porting a driver to another kernel only requires
//! implementing the providers, not rewriting the driver.
#![no_std]

extern crate alloc;

mod dma;
mod irq;
mod mmio;
mod provider;

use alloc::boxed::Box;

/// A single resource associated with a device.
#[derive(Debug, Clone, Copy)]
pub enum ResourceDesc {
    /// Memory-mapped I/O region.
    Mmio(crate::mmio::MmioRegion),
    /// x86 I/O port range.
    IoPort(crate::mmio::IoPortRange),
    /// Interrupt line.
    Irq(crate::irq::IrqResource),
    /// Coherent DMA buffer request.
    Dma(crate::dma::DmaSpec),
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

/// OS-agnostic view of a device that a driver binds resources to.
///
/// A host kernel implements this for its concrete device object so the
/// device-managed (`devm_*`) helpers and driver code can read the discovered
/// resources and attach resource teardown without depending on a kernel type.
/// Drivers can therefore be written against `&dyn DeviceResource` and ported
/// by reimplementing only the host adapter and the three capability providers.
pub trait DeviceResource {
    /// The hardware resources discovered for this device.
    fn resources(&self) -> &[ResourceDesc];

    /// Register a teardown callback bound to the device's lifetime.
    ///
    /// Callbacks run in reverse (LIFO) order when the device's probe fails or
    /// when the device is removed.
    fn register_cleanup(&self, cleanup: Box<dyn FnOnce() + Send>);
}

pub use dma::*;
pub use irq::*;
pub use mmio::*;
pub use provider::*;

#[cfg(unittest)]
mod tests {
    //! Unit tests for the RAII handles and `devm_*` helpers.
    //!
    //! A mock triple of providers (`MmioOp` / `DmaOp` / `IrqOp`) records every
    //! call into an event log backed by real memory, so the drop semantics and the
    //! `devm_*` LIFO cleanup ordering can be asserted exactly. The provider
    //! registry is replaceable under `--cfg unittest`, so each test swaps in the
    //! mock even though the host kernel has already installed the real providers
    //! during early init.

    use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};

    use kspin::SpinNoIrq;
    use unittest::def_test;

    use crate::{
        DeviceResource, DmaAllocation, DmaCoherent, DmaOp, DmaSpec, Io, Irq, IrqEvent, IrqHandler,
        IrqOp, IrqResource, IrqTriggerMode, MmioMapping, MmioOp, MmioRegion, ResError, ResResult,
        ResourceDesc, devm_alloc_coherent, devm_iomap, devm_request_irq, provider_installed,
        reset_providers, set_dma_provider, set_irq_provider, set_mmio_provider, try_dma_provider,
        try_irq_provider, try_mmio_provider,
    };

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

    impl MmioOp for TestProvider {
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
    }

    impl DmaOp for TestProvider {
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

    impl IrqOp for TestProvider {
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

    /// RAII guard that installs the mock provider triple on creation and
    /// restores the previously-installed providers on drop. Restoring the
    /// registry after each test keeps the host backend in place for later
    /// kernel code (e.g. virtio probing) that shares the same global registry.
    struct ProviderGuard {
        saved_mmio: Option<&'static dyn MmioOp>,
        saved_dma: Option<&'static dyn DmaOp>,
        saved_irq: Option<&'static dyn IrqOp>,
    }

    impl ProviderGuard {
        fn new() -> Self {
            let guard = Self {
                saved_mmio: try_mmio_provider(),
                saved_dma: try_dma_provider(),
                saved_irq: try_irq_provider(),
            };
            set_mmio_provider(&TEST_PROVIDER);
            set_dma_provider(&TEST_PROVIDER);
            set_irq_provider(&TEST_PROVIDER);
            TEST_PROVIDER.reset();
            guard
        }
    }

    impl Drop for ProviderGuard {
        fn drop(&mut self) {
            if let Some(p) = self.saved_mmio {
                set_mmio_provider(p);
            }
            if let Some(p) = self.saved_dma {
                set_dma_provider(p);
            }
            if let Some(p) = self.saved_irq {
                set_irq_provider(p);
            }
        }
    }

    #[def_test]
    fn missing_provider_returns_no_provider_error() {
        let _serial = TEST_SERIAL.lock();
        let _g = ProviderGuard::new();
        reset_providers();
        assert!(!provider_installed());

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
        assert_eq!(
            Irq::request(
                IrqResource::new(1, IrqTriggerMode::EdgeRising),
                Arc::new(|| IrqEvent::HANDLED),
            )
            .unwrap_err(),
            ResError::NoProvider
        );
        assert_eq!(
            DmaCoherent::alloc(DmaSpec::new(16, 8)).unwrap_err(),
            ResError::NoProvider
        );
    }

    #[def_test]
    fn io_read_write_and_drop_follow_provider_lifecycle() {
        let _serial = TEST_SERIAL.lock();
        let _g = ProviderGuard::new();

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
        let _g = ProviderGuard::new();

        let irq = IrqResource::new(7, IrqTriggerMode::LevelHigh);
        let spec = DmaSpec::new(24, 8);

        let guard = Irq::request(irq, Arc::new(|| IrqEvent::HANDLED)).unwrap();
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

        devm_request_irq(&device, irq, Arc::new(|| IrqEvent::HANDLED)).unwrap();
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
        let _g = ProviderGuard::new();

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
                IrqResource::new(9, IrqTriggerMode::EdgeRising),
                Arc::new(|| IrqEvent::NOT_HANDLED),
            )
            .unwrap_err(),
            ResError::Busy
        );

        TEST_PROVIDER.reset();
        TEST_PROVIDER.set_dma_error(ResError::NoMemory);
        assert_eq!(
            devm_alloc_coherent(&device, DmaSpec::new(8, 8)).unwrap_err(),
            ResError::NoMemory
        );
    }
}
