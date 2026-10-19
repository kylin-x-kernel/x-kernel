// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktask::WaitQueue;
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn test_is_virtio_rng_name_matching() {
    assert!(is_virtio_rng("virtio-rng", "unknown"));
    assert!(is_virtio_rng("hwrng0", "virtio-rng"));
    assert!(is_virtio_rng("rng", "platform-virtio-rng-pci"));
    assert!(!is_virtio_rng("ttyS0", "uart"));
    assert!(!is_virtio_rng("virtio-blk", "virtio-blk"));
}

use core::sync::atomic::{AtomicUsize, Ordering};

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
        "virtio-rng-test"
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

    fn match_desc(&self, _: &dyn DeviceDriver, _: &DeviceDesc) -> MatchResult {
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

    fn probe_device(&self, _: Arc<kdevice::DeviceObject>) -> Result<(), DriverError> {
        Err(DriverError::Unsupported)
    }
}

#[def_test(serial)]
fn collector_retries_without_new_demand_and_stops_only_on_device_failure() {
    kdevice::register_bus_type(Arc::new(kdevice::BusTypeObject::new(Arc::new(Bus))));
    let bus = kdevice::register_bus_instance(BUS, "entropy-collector-test");
    let driver = kdevice::register_driver_object(Arc::new(Driver));
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
            driver: driver.clone(),
        })
        .unwrap();
        let hardware = Arc::new(Hardware {
            state: Arc::new(SpinNoIrq::new(HardwareState::default())),
            reads: AtomicUsize::new(0),
            ready: WaitQueue::new(),
            entered: WaitQueue::new(),
        });
        let class = kclass::publish_hwrng(parent.clone(), hardware.clone()).unwrap();
        let source = VirtioEntropySource::new(class).unwrap();
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
