// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! OS-agnostic device resource model and capability abstraction.
//!
//! This crate describes the hardware resources a driver consumes (MMIO
//! regions, I/O port ranges, interrupts, coherent/streaming DMA buffers, and
//! small execution capabilities such as a monotonic clock)
//! without binding to any particular kernel. A host kernel provides capability
//! implementations — [`MmioOp`], [`DmaOp`], [`IrqOp`], [`TimeOp`] — to its
//! driver framework; framework code then acquires resources through RAII
//! handles ([`Io`], [`Irq`], [`DmaCoherent`]) or the device-managed `devm_*`
//! helpers.
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
mod time;

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
pub use time::*;

#[cfg(unittest)]
mod tests {
    //! Unit tests for resource provider contracts and device cleanup ordering.
    //!
    //! A mock triple of providers (`MmioOp` / `DmaOp` / `IrqOp`) records every
    //! call into an event log backed by real memory. Tests exercise explicit
    //! provider calls so resource acquire/release pairing stays visible.

    use alloc::{boxed::Box, sync::Arc, vec, vec::Vec};
    use core::sync::atomic::{AtomicUsize, Ordering};

    use kspin::SpinNoIrq;
    use unittest::def_test;

    use crate::{
        DeviceResource, DmaAllocation, DmaOp, DmaSpec, IrqEvent, IrqHandler, IrqHandlerToken,
        IrqOp, IrqResource, IrqThreadHandler, IrqTrigger, MmioMapping, MmioOp, MmioRegion,
        ResError, ResResult, ResourceDesc, devm_alloc_coherent_with_provider,
        devm_iomap_with_provider, devm_request_irq_with_provider,
    };

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Event {
        MapMmio(MmioRegion),
        UnmapMmio(MmioRegion),
        RequestIrq(IrqResource),
        RequestThreadedIrq(IrqResource),
        RequestThreadedIrqDefault(IrqResource),
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
        fn request_irq(
            &self,
            irq: IrqResource,
            _handler: Arc<dyn IrqHandler>,
        ) -> ResResult<IrqHandlerToken> {
            let mut state = self.state.lock();
            state.events.push(Event::RequestIrq(irq));
            if let Some(error) = state.irq_error.take() {
                return Err(error);
            }
            Ok(IrqHandlerToken::shared_action(1))
        }

        fn request_threaded_irq(
            &self,
            irq: IrqResource,
            _primary: Arc<dyn IrqHandler>,
            _thread: Arc<dyn IrqThreadHandler>,
            _name: Option<&'static str>,
        ) -> ResResult<IrqHandlerToken> {
            let mut state = self.state.lock();
            state.events.push(Event::RequestThreadedIrq(irq));
            if let Some(error) = state.irq_error.take() {
                return Err(error);
            }
            Ok(IrqHandlerToken::regular_action())
        }

        fn request_threaded_irq_default(
            &self,
            irq: IrqResource,
            _thread: Arc<dyn IrqThreadHandler>,
            _name: Option<&'static str>,
        ) -> ResResult<IrqHandlerToken> {
            let mut state = self.state.lock();
            state.events.push(Event::RequestThreadedIrqDefault(irq));
            if let Some(error) = state.irq_error.take() {
                return Err(error);
            }
            Ok(IrqHandlerToken::regular_action())
        }

        fn release_irq(&self, irq: IrqResource, _token: IrqHandlerToken) {
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

    #[def_test]
    fn provider_methods_record_lifecycle_with_explicit_calls() {
        let provider = TestProvider::new();

        let region = MmioRegion {
            base: 0x2000,
            size: 16,
        };
        let irq = IrqResource::new(7, IrqTrigger::LevelHigh);
        let spec = DmaSpec::new(24, 8);

        let mapping = provider.map_mmio(region, "regs").unwrap();
        assert_eq!(mapping.region, region);
        provider.unmap_mmio(mapping);

        let token = provider
            .request_irq(irq, Arc::new(|_| IrqEvent::HANDLED))
            .unwrap();
        provider.set_irq_enabled(irq, false);
        provider.release_irq(irq, token);
        let threaded_token = provider
            .request_threaded_irq(
                irq,
                Arc::new(|_| IrqEvent::WAKE_THREAD),
                Arc::new(|_| IrqEvent::HANDLED),
                Some("threaded"),
            )
            .unwrap();
        provider.release_irq(irq, threaded_token);
        let default_threaded_token = provider
            .request_threaded_irq_default(irq, Arc::new(|_| IrqEvent::HANDLED), Some("default"))
            .unwrap();
        provider.release_irq(irq, default_threaded_token);

        let dma = provider.alloc_coherent(spec).unwrap();
        assert_eq!(dma.bus_addr, 0xfeed_cafe);
        provider.free_coherent(dma);

        let device = TestDevice::new(vec![ResourceDesc::Irq(irq), ResourceDesc::Dma(spec)]);
        assert_eq!(device.resources().len(), 2);

        assert_eq!(
            provider.events(),
            vec![
                Event::MapMmio(region),
                Event::UnmapMmio(region),
                Event::RequestIrq(irq),
                Event::SetIrqEnabled(irq, false),
                Event::ReleaseIrq(irq),
                Event::RequestThreadedIrq(irq),
                Event::ReleaseIrq(irq),
                Event::RequestThreadedIrqDefault(irq),
                Event::ReleaseIrq(irq),
                Event::AllocCoherent(spec),
                Event::FreeCoherent(spec),
            ]
        );
    }

    #[def_test]
    fn provider_errors_propagate_from_explicit_calls() {
        let provider = TestProvider::new();
        let region = MmioRegion {
            base: 0x3000,
            size: 8,
        };
        provider.set_map_error(ResError::MappingFailed);
        assert_eq!(
            provider.map_mmio(region, "failing").unwrap_err(),
            ResError::MappingFailed
        );

        provider.reset();
        provider.set_irq_error(ResError::Busy);
        assert_eq!(
            provider
                .request_irq(
                    IrqResource::new(9, IrqTrigger::EdgeRising),
                    Arc::new(|_| IrqEvent::NOT_HANDLED),
                )
                .unwrap_err(),
            ResError::Busy
        );

        provider.reset();
        provider.set_irq_error(ResError::Unsupported);
        assert_eq!(
            provider
                .request_threaded_irq_default(
                    IrqResource::new(10, IrqTrigger::LevelLow),
                    Arc::new(|_| IrqEvent::HANDLED),
                    Some("unsupported"),
                )
                .unwrap_err(),
            ResError::Unsupported
        );

        provider.reset();
        provider.set_dma_error(ResError::NoMemory);
        assert_eq!(
            provider.alloc_coherent(DmaSpec::new(8, 8)).unwrap_err(),
            ResError::NoMemory
        );
    }

    #[def_test]
    fn device_cleanups_run_lifo_order() {
        static ORDER: SpinNoIrq<Vec<usize>> = SpinNoIrq::new(Vec::new());
        static NEXT: AtomicUsize = AtomicUsize::new(0);

        ORDER.lock().clear();
        NEXT.store(0, Ordering::Relaxed);

        let device = TestDevice::new(Vec::new());
        for id in 0..3 {
            device.register_cleanup(Box::new(move || {
                let seq = NEXT.fetch_add(1, Ordering::Relaxed);
                ORDER.lock().push((seq << 8) | id);
            }));
        }

        device.run_cleanups();

        assert_eq!(&*ORDER.lock(), &[2, 0x101, 0x200]);
    }

    #[def_test]
    fn devm_helpers_use_explicit_provider_for_cleanup() {
        static PROVIDER: TestProvider = TestProvider::new();

        PROVIDER.reset();
        let region = MmioRegion {
            base: 0x4000,
            size: 32,
        };
        let irq = IrqResource::new(11, IrqTrigger::LevelHigh);
        let spec = DmaSpec::new(16, 8);
        let device = TestDevice::new(vec![
            ResourceDesc::Mmio(region),
            ResourceDesc::Irq(irq),
            ResourceDesc::Dma(spec),
        ]);

        let ptr = devm_iomap_with_provider(&PROVIDER, &device, region, "explicit").unwrap();
        assert_eq!(
            ptr.as_ptr() as usize,
            PROVIDER.state.lock().mmio_words.as_ptr() as usize
        );
        devm_request_irq_with_provider(&PROVIDER, &device, irq, Arc::new(|_| IrqEvent::HANDLED))
            .unwrap();
        let (cpu_addr, bus_addr) =
            devm_alloc_coherent_with_provider(&PROVIDER, &device, spec).unwrap();
        assert_eq!(
            cpu_addr.as_ptr() as usize,
            PROVIDER.state.lock().dma.as_ptr() as usize
        );
        assert_eq!(bus_addr, 0xfeed_cafe);

        device.run_cleanups();

        assert_eq!(
            PROVIDER.events(),
            vec![
                Event::MapMmio(region),
                Event::RequestIrq(irq),
                Event::AllocCoherent(spec),
                Event::FreeCoherent(spec),
                Event::ReleaseIrq(irq),
                Event::UnmapMmio(region),
            ]
        );
    }
}
