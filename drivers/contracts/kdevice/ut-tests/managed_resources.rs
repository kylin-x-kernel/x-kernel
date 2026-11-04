// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for resource provider contracts and device cleanup ordering.
//!
//! A mock triple of providers (`MmioOp` / `DmaOp` / `IrqOp`) records every
//! call into an event log backed by real memory. Tests exercise explicit
//! provider calls so resource acquire/release pairing stays visible.

use alloc::{sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicUsize, Ordering};

use driver_os_api::{
    DmaAllocation, DmaOp, DmaSpec, IrqEvent, IrqHandler, IrqHandlerToken, IrqOp, IrqResource,
    IrqThreadHandler, IrqTrigger, MmioMapping, MmioOp, MmioRegion, ResError, ResResult,
};
use kspin::SpinNoIrq;
use unittest::def_test;

use super::{
    devm_alloc_coherent_with_provider, devm_iomap_with_provider, devm_request_irq_with_provider,
};
use crate::{DeviceObject, ResourceDesc};

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

// SAFETY: fixtures retain stable backing words for each mapping until cleanup;
// the state lock serializes access and unmap only records reference release.
unsafe impl MmioOp for TestProvider {
    fn map_mmio(&self, region: MmioRegion, _name: &'static str) -> ResResult<MmioMapping> {
        let mut state = self.state.lock();
        state.events.push(Event::MapMmio(region));
        if let Some(error) = state.map_error.take() {
            return Err(error);
        }
        if region.size == 0 || region.size > core::mem::size_of_val(&state.mmio_words) {
            return Err(ResError::InvalidResource);
        }
        Ok(MmioMapping {
            vaddr: state.mmio_words.as_mut_ptr() as usize,
            region,
        })
    }

    unsafe fn unmap_mmio(&self, mapping: MmioMapping) {
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

fn test_device(resources: Vec<ResourceDesc>) -> DeviceObject {
    DeviceObject::new(
        crate::DeviceId::new(0),
        crate::BusId::new(0),
        crate::DeviceLocation::PlatformStatic { id: 0 },
        crate::DiscoveryOrigin::PlatformStatic,
        crate::DeviceIdentity::Platform(crate::PlatformIdentity {
            alias: Some("managed-resource-test"),
            firmware_id: None,
        }),
        None,
        resources.into_iter().collect(),
    )
}

#[def_test]
fn provider_methods_record_lifecycle_with_explicit_calls() {
    let provider = TestProvider::new();

    let region = MmioRegion {
        base: 0x2000,
        size: 16,
    };
    let irq = IrqResource::plain(driver_os_api::Virq::new(7), IrqTrigger::LevelHigh);
    let spec = DmaSpec::new(24, 8);

    let mapping = provider.map_mmio(region, "regs").unwrap();
    assert_eq!(mapping.region, region);
    // SAFETY: the token belongs to this provider and no access remains active.
    unsafe { provider.unmap_mmio(mapping) };

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

    let device = test_device(vec![ResourceDesc::Irq(irq), ResourceDesc::Dma(spec)]);
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
                IrqResource::plain(driver_os_api::Virq::new(9), IrqTrigger::EdgeRising),
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
                IrqResource::plain(driver_os_api::Virq::new(10), IrqTrigger::LevelLow),
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
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: Dylint lint is unknown to Clippy"
    )]
    #[allow(
        unknown_lints,
        unregistered_static_spinlock,
        reason = "CG-001-static-spinlock-registration: test-only lock; not tracked in lock_stat"
    )]
    static ORDER: SpinNoIrq<Vec<usize>> = SpinNoIrq::new(Vec::new());
    static NEXT: AtomicUsize = AtomicUsize::new(0);

    ORDER.lock().clear();
    NEXT.store(0, Ordering::Relaxed);

    let device = test_device(Vec::new());
    for id in 0..3 {
        device.add_cleanup(move || {
            let seq = NEXT.fetch_add(1, Ordering::Relaxed);
            ORDER.lock().push((seq << 8) | id);
        });
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
    let irq = IrqResource::plain(driver_os_api::Virq::new(11), IrqTrigger::LevelHigh);
    let spec = DmaSpec::new(16, 8);
    let device = test_device(vec![
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
    let (cpu_addr, bus_addr) = devm_alloc_coherent_with_provider(&PROVIDER, &device, spec).unwrap();
    assert_eq!(
        cpu_addr.as_ptr() as usize,
        PROVIDER.state.lock().dma.as_ptr() as usize
    );
    assert_eq!(bus_addr, 0xfeed_cafe);

    device.run_cleanups();
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

#[def_test]
fn failed_managed_acquisitions_register_no_cleanup() {
    static PROVIDER: TestProvider = TestProvider::new();
    PROVIDER.reset();
    let device = test_device(Vec::new());
    let region = MmioRegion {
        base: 0x5000,
        size: 8,
    };
    let irq = IrqResource::plain(driver_os_api::Virq::new(12), IrqTrigger::LevelHigh);
    let spec = DmaSpec::new(8, 8);
    PROVIDER.set_map_error(ResError::MappingFailed);
    assert_eq!(
        devm_iomap_with_provider(&PROVIDER, &device, region, "failure"),
        Err(ResError::MappingFailed)
    );
    PROVIDER.set_irq_error(ResError::Busy);
    assert_eq!(
        devm_request_irq_with_provider(&PROVIDER, &device, irq, Arc::new(|_| IrqEvent::HANDLED)),
        Err(ResError::Busy)
    );
    PROVIDER.set_dma_error(ResError::NoMemory);
    assert_eq!(
        devm_alloc_coherent_with_provider(&PROVIDER, &device, spec),
        Err(ResError::NoMemory)
    );
    device.run_cleanups();
    assert_eq!(
        PROVIDER.events(),
        vec![
            Event::MapMmio(region),
            Event::RequestIrq(irq),
            Event::AllocCoherent(spec)
        ]
    );
}

#[def_test]
fn managed_threaded_guards_release_once_after_success_only() {
    static PROVIDER: TestProvider = TestProvider::new();
    PROVIDER.reset();
    let device = test_device(Vec::new());
    let irq = IrqResource::plain(driver_os_api::Virq::new(13), IrqTrigger::LevelHigh);
    super::devm_request_threaded_irq_with_provider(
        &PROVIDER,
        &device,
        irq,
        Arc::new(|_| IrqEvent::WAKE_THREAD),
        Arc::new(|_| IrqEvent::HANDLED),
        Some("primary"),
    )
    .unwrap();
    super::devm_request_threaded_irq_default_with_provider(
        &PROVIDER,
        &device,
        irq,
        Arc::new(|_| IrqEvent::HANDLED),
        Some("default"),
    )
    .unwrap();
    PROVIDER.set_irq_error(ResError::Unsupported);
    assert_eq!(
        super::devm_request_threaded_irq_default_with_provider(
            &PROVIDER,
            &device,
            irq,
            Arc::new(|_| IrqEvent::HANDLED),
            None
        ),
        Err(ResError::Unsupported)
    );
    device.run_cleanups();
    device.run_cleanups();
    assert_eq!(
        PROVIDER.events(),
        vec![
            Event::RequestThreadedIrq(irq),
            Event::RequestThreadedIrqDefault(irq),
            Event::RequestThreadedIrqDefault(irq),
            Event::ReleaseIrq(irq),
            Event::ReleaseIrq(irq)
        ]
    );
}
