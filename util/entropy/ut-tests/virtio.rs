// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktask::WaitQueue;
use unittest::{assert, assert_eq, def_test};

use super::*;
use kclass::RevocationOutcome;

#[def_test]
fn test_is_virtio_rng_name_matching() {
    assert!(is_virtio_rng("virtio-rng", "unknown"));
    assert!(is_virtio_rng("hwrng0", "virtio-rng"));
    assert!(is_virtio_rng("rng", "platform-virtio-rng-pci"));
    assert!(!is_virtio_rng("ttyS0", "uart"));
    assert!(!is_virtio_rng("virtio-blk", "virtio-blk"));
}

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use hwrng::{Device, DeviceKind, DriverError, DriverResult};
use kdevice::{BusTypeId, DeviceDesc, DeviceDriver, MatchResult};

#[derive(Default)]
struct HardwareState {
    is_ready: bool,
    fault: Option<DriverError>,
    is_failed: bool,
}
struct Hardware {
    state: Arc<SpinNoIrq<HardwareState>>,
    reads: AtomicUsize,
    ready: WaitQueue,
    entered: WaitQueue,
}
impl Hardware {
    fn wait_reads(&self, count: usize) {
        self.entered
            .wait_until(|| self.reads.load(Ordering::Acquire) >= count);
    }

    fn complete(&self) {
        self.state.lock().is_ready = true;
        self.ready.notify_all(false);
    }

    fn fail(&self, error: DriverError) {
        self.state.lock().fault = Some(error);
        self.ready.notify_all(false);
    }
}
impl Device for Hardware {
    fn name(&self) -> &str {
        "virtio-rng"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Hwrng
    }
}
impl HwrngDevice for Hardware {
    fn is_failed(&self) -> bool {
        self.state.lock().is_failed
    }

    fn read(&self, output: &mut [u8]) -> DriverResult<usize> {
        self.reads.fetch_add(1, Ordering::Release);
        self.entered.notify_all(false);
        self.ready.wait_until(|| {
            let state = self.state.lock();
            state.is_ready || state.fault.is_some()
        });
        let mut state = self.state.lock();
        if let Some(error) = state.fault.take() {
            return Err(error);
        }
        state.is_ready = false;
        output.fill(0x5a);
        Ok(output.len())
    }
}
const BUS: BusTypeId = BusTypeId::new("entropy-collector-test");
struct Bus;
impl kdevice::BusType for Bus {
    fn id(&self) -> BusTypeId {
        BUS
    }

    fn match_desc(&self, _: &dyn kdevice::DriverMatchInfo, _: &DeviceDesc) -> MatchResult {
        MatchResult::NoMatch
    }
}
struct Driver;
impl kdevice::DeviceMatcher for Driver {
    fn matches(&self, _: &DeviceDesc) -> MatchResult {
        MatchResult::NoMatch
    }
}
impl DeviceDriver for Driver {
    type Data = ();
    fn name(&self) -> &'static str {
        "entropy-collector-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Hwrng
    }

    fn bus_types(&self) -> &'static [BusTypeId] {
        &[BUS]
    }

    fn matcher(&self) -> &dyn kdevice::DeviceMatcher {
        self
    }

    fn probe_device(&self, _: &kdevice::ProbeContext<'_>) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }
}

#[def_test(serial)]
fn collector_retries_without_new_demand_and_stops_only_on_device_failure() {
    kdevice::register_bus_type(Arc::new(kdevice::BusTypeObject::new(Arc::new(Bus))));
    let bus = kdevice::register_bus_instance(BUS, "entropy-collector-test");
    let driver = kdevice::register_driver_object(kdevice::BoxedDriver::new_adoptable(Driver));
    for index in 0..3 {
        let parent = kdevice::adopt_active_device(kdevice::ActiveDeviceAdoption {
            bus_id: bus.id(),
            parent: None,
            location: kdevice::DeviceLocation::PlatformStatic { id: index },
            origin: kdevice::DiscoveryOrigin::PlatformStatic,
            identity: kdevice::DeviceIdentity::Platform(kdevice::PlatformIdentity {
                alias: Some("collector-test"),
                firmware_id: None,
            }),
            transport: None,
            resources: kdevice::ResourceSet::new(),
            dma: None,
            driver: driver.clone(),
        })
        .unwrap();
        let hardware = Arc::new(Hardware {
            state: Arc::new(SpinNoIrq::new(HardwareState::default())),
            reads: AtomicUsize::new(0),
            ready: WaitQueue::new(),
            entered: WaitQueue::new(),
        });
        let class = kdevice::test_hooks::with_binding_lock(parent.id(), || {
            kdevice::ProbeContext::with_test_context(parent.clone(), |ctx| {
                kclass::publish_hwrng(ctx, hardware.clone())
            })
            .expect("the adopted device has a probe context")
        })
        .expect("the adopted device still has a binding")
        .unwrap();
        let source = VirtioEntropySource::new(class.device()).unwrap();
        assert_eq!(hardware.reads.load(Ordering::Acquire), 0);
        source.request();
        hardware.wait_reads(1);
        // Requests while the first read is blocked coalesce into one follow-up.
        for _ in 0..10 {
            source.request();
        }
        hardware.complete();
        hardware.wait_reads(2);
        hardware.complete();
        source.work.flush().unwrap();
        assert_eq!(hardware.reads.load(Ordering::Acquire), 2);
        assert!(!source.collector.state.lock().has_demand);

        // The same prepared Work accepts a fresh demand after returning idle.
        source.request();
        hardware.wait_reads(3);
        let error = match index {
            0 => DriverError::WouldBlock,
            1 => DriverError::NoMemory,
            _ => DriverError::InvalidInput,
        };
        // Retry without a new demand, beyond the old three-error cutoff.
        for attempt in 0..4 {
            hardware.fail(error);
            hardware.wait_reads(4 + attempt);
            assert!(!source.collector.state.lock().is_failed);
        }
        hardware.complete();
        source.work.flush().unwrap();
        assert_eq!(hardware.reads.load(Ordering::Acquire), 7);

        hardware.state.lock().is_failed = true;
        hardware.fail(DriverError::Io);
        source.request();
        source.work.flush().unwrap();
        assert!(source.collector.state.lock().is_failed);
        let reads = hardware.reads.load(Ordering::Acquire);
        source.request();
        source.work.flush().unwrap();
        assert_eq!(hardware.reads.load(Ordering::Acquire), reads);
        source.work.shutdown().unwrap();
        kdevice::remove_device_managed(parent.id()).unwrap();
    }
}

#[def_test(serial)]
fn selected_source_is_publication_scoped_and_survives_stale_removal() {
    kdevice::register_bus_type(Arc::new(kdevice::BusTypeObject::new(Arc::new(Bus))));
    let bus = kdevice::register_bus_instance(BUS, "entropy-collector-test");
    let driver = kdevice::register_driver_object(kdevice::BoxedDriver::new_adoptable(Driver));
    let parent = kdevice::adopt_active_device(kdevice::ActiveDeviceAdoption {
        bus_id: bus.id(),
        parent: None,
        location: kdevice::DeviceLocation::PlatformStatic { id: 0xE1 },
        origin: kdevice::DiscoveryOrigin::PlatformStatic,
        identity: kdevice::DeviceIdentity::Platform(kdevice::PlatformIdentity {
            alias: Some("collector-test"),
            firmware_id: None,
        }),
        transport: None,
        resources: kdevice::ResourceSet::new(),
        dma: None,
        driver: driver.clone(),
    })
    .unwrap();
    let hardware = Arc::new(Hardware {
        state: Arc::new(SpinNoIrq::new(HardwareState::default())),
        reads: AtomicUsize::new(0),
        ready: WaitQueue::new(),
        entered: WaitQueue::new(),
    });
    let first = kdevice::test_hooks::with_binding_lock(parent.id(), || {
        kdevice::ProbeContext::with_test_context(parent.clone(), |ctx| {
            kclass::publish_hwrng(ctx, hardware.clone())
        })
        .expect("probe context")
    })
    .expect("binding")
    .unwrap();
    let second = kdevice::test_hooks::with_binding_lock(parent.id(), || {
        kdevice::ProbeContext::with_test_context(parent.clone(), |ctx| {
            kclass::publish_hwrng(ctx, hardware.clone())
        })
        .expect("probe context")
    })
    .expect("binding")
    .unwrap();
    kclass::activate_hwrng_for_test(parent.id());
    *SELECTED_SOURCE.lock() = None;
    note_device(first.device());
    assert!(
        SELECTED_SOURCE
            .lock()
            .as_ref()
            .is_some_and(|source| source.publication_id() == first.publication_id())
    );
    assert_eq!(
        first.revoke().expect("first revoke completes"),
        RevocationOutcome::Completed
    );
    note_device(second.device());
    let second_source = SELECTED_SOURCE
        .lock()
        .as_ref()
        .cloned()
        .expect("second source selected");
    assert_eq!(second_source.publication_id(), second.publication_id());

    remove_source(first.publication_id());
    assert!(
        SELECTED_SOURCE
            .lock()
            .as_ref()
            .is_some_and(|source| source.publication_id() == second.publication_id())
    );
    assert!(second_source.collector.device.is_available());

    assert!(second_source.collector.device.is_available());
    remove_source(second.publication_id());
    assert!(SELECTED_SOURCE.lock().is_none());
    assert!(second_source.collector.is_failed());
    assert!(second_source.collector.device.is_available());

    assert_eq!(
        second.revoke().expect("publication revoke completes"),
        RevocationOutcome::Completed
    );
    assert!(!second_source.collector.device.is_available());

    kdevice::remove_device_managed(parent.id()).unwrap();
}

/// A failed read pauses at the retry boundary; revoking there prevents the
/// next read deterministically.
#[def_test(serial)]
fn retry_wait_rechecks_revocation_before_next_read() {
    kdevice::register_bus_type(Arc::new(kdevice::BusTypeObject::new(Arc::new(Bus))));
    let bus = kdevice::register_bus_instance(BUS, "entropy-collector-test");
    let driver = kdevice::register_driver_object(kdevice::BoxedDriver::new_adoptable(Driver));
    let parent = kdevice::adopt_active_device(kdevice::ActiveDeviceAdoption {
        bus_id: bus.id(),
        parent: None,
        location: kdevice::DeviceLocation::PlatformStatic { id: 0xE2 },
        origin: kdevice::DiscoveryOrigin::PlatformStatic,
        identity: kdevice::DeviceIdentity::Platform(kdevice::PlatformIdentity {
            alias: Some("collector-test"),
            firmware_id: None,
        }),
        transport: None,
        resources: kdevice::ResourceSet::new(),
        dma: None,
        driver: driver.clone(),
    })
    .unwrap();
    let hardware = Arc::new(Hardware {
        state: Arc::new(SpinNoIrq::new(HardwareState::default())),
        reads: AtomicUsize::new(0),
        ready: WaitQueue::new(),
        entered: WaitQueue::new(),
    });
    let publication = Arc::new(
        kdevice::test_hooks::with_binding_lock(parent.id(), || {
            kdevice::ProbeContext::with_test_context(parent.clone(), |ctx| {
                kclass::publish_hwrng(ctx, hardware.clone())
            })
            .expect("probe context")
        })
        .expect("binding")
        .unwrap(),
    );
    let retry_paused = Arc::new(AtomicBool::new(false));
    let release_retry = Arc::new(AtomicBool::new(false));
    {
        let retry_paused = retry_paused.clone();
        let release_retry = release_retry.clone();
        *RETRY_PAUSE.lock() = Some(Arc::new(move || {
            retry_paused.store(true, Ordering::SeqCst);
            while !release_retry.load(Ordering::SeqCst) {
                ktask::yield_now();
            }
        }));
    }
    let source = VirtioEntropySource::new(publication.device()).unwrap();
    source.request();
    hardware.wait_reads(1);
    hardware.fail(DriverError::WouldBlock);
    let mut paused = false;
    for _ in 0..100_000 {
        if retry_paused.load(Ordering::SeqCst) {
            paused = true;
            break;
        }
        ktask::yield_now();
    }
    let revoke_result = paused.then(|| publication.revoke());
    release_retry.store(true, Ordering::SeqCst);
    hardware.complete();
    let flush_result = source.work.flush();
    *RETRY_PAUSE.lock() = None;
    assert!(paused, "worker never reached the retry pause");
    assert_eq!(
        revoke_result.expect("retry pause was reached").expect("revoke completes"),
        RevocationOutcome::Completed
    );
    flush_result.unwrap();
    assert_eq!(hardware.reads.load(Ordering::Acquire), 1);
    assert!(!publication.is_available());
    kdevice::remove_device_managed(parent.id()).unwrap();
}
