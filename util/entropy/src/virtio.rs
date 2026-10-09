// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO demand-driven entropy collection (requires Host trust).

use alloc::{boxed::Box, sync::Arc};

use hwrng::HwrngDevice;
use kclass::{ClassDevice, HwrngDeviceImpl, hwrng_devices, subscribe_hwrng_available};
use klazy::Lazy;
use kspin::SpinNoIrq;
use ksync::Mutex;
use kworkng::{HandleCreateError, SubmitError, WorkHandle};

// Back off failed reads, not in-flight device completion waits.
const READ_RETRY_DELAY: ktime_types::TimeSpan = ktime_types::TimeSpan::from_millis(100);

static SELECTED_SOURCE: Lazy<Mutex<Option<Arc<VirtioEntropySource>>>> =
    Lazy::new(|| Mutex::new(None));

type CollectorWork = WorkHandle<Box<dyn Fn() + Send + Sync>>;

struct VirtioEntropySource {
    collector: Arc<EntropyCollector>,
    work: CollectorWork,
}

// Work owns the collector, which never owns Work: no reference cycle is formed.
struct EntropyCollector {
    device: ClassDevice<HwrngDeviceImpl>,
    state: SpinNoIrq<CollectorState>,
}

struct CollectorState {
    /// A coalesced read request waiting for the worker.
    has_demand: bool,
    /// A terminal read failure prevents further requests.
    is_failed: bool,
}

impl VirtioEntropySource {
    fn new(device: ClassDevice<HwrngDeviceImpl>) -> Result<Arc<Self>, HandleCreateError> {
        let collector = Arc::new(EntropyCollector {
            device,
            state: SpinNoIrq::new(CollectorState {
                has_demand: false,
                is_failed: false,
            }),
        });
        let callback = collector.clone();
        let work =
            WorkHandle::new(Box::new(move || callback.run()) as Box<dyn Fn() + Send + Sync>)?;
        Ok(Arc::new(Self { collector, work }))
    }

    fn request(&self) {
        let mut state = self.collector.state.lock();
        if state.is_failed {
            return;
        }
        let had_demand = state.has_demand;
        state.has_demand = true;
        // Publish demand and enqueue under one lock. A running callback can
        // acquire one pending occurrence, including during its return window.
        // AlreadyQueued coalesces requests; work is never an event counter.
        match self.work.queue_default(None) {
            Ok(_) | Err(SubmitError::AlreadyQueued) => {}
            Err(error) => {
                state.has_demand = had_demand;
                drop(state);
                log::warn!("entropy: failed to queue VirtIO entropy work: {error:?}");
            }
        }
    }
}

impl EntropyCollector {
    fn fail(&self) {
        let mut state = self.state.lock();
        state.is_failed = true;
        state.has_demand = false;
    }

    fn run(&self) {
        {
            let mut state = self.state.lock();
            if state.is_failed || !state.has_demand {
                return;
            }
            state.has_demand = false;
        }
        let mut bytes = [0; super::HW_RESEED_BYTES];
        let mut has_reported_error = false;
        // Retain this demand across errors: a getrandom caller may already
        // be asleep and cannot be relied on to request another sample.
        loop {
            match self.device.read(&mut bytes) {
                Ok(len) if len > 0 && len <= bytes.len() => {
                    super::mix_virtio_entropy(&bytes[..len]);
                    break;
                }
                Err(error) => {
                    if self.device.is_failed() {
                        log::warn!("entropy: VirtIO entropy device failed: {error:?}");
                        self.fail();
                        return;
                    }
                    if !has_reported_error {
                        log::warn!("entropy: retrying VirtIO entropy read: {error:?}");
                        has_reported_error = true;
                    }
                    // This shared worker holds no device/source lock.
                    // Failed wait setup must neither offline the source nor
                    // turn a persistent host error into a busy loop.
                    ktask::sleep(READ_RETRY_DELAY);
                }
                Ok(len) => {
                    log::warn!(
                        "entropy: permanently disabling VirtIO entropy source after invalid read \
                         length: {len} (expected 1..={})",
                        bytes.len()
                    );
                    self.fail();
                    return;
                }
            }
        }
    }
}

pub(crate) fn is_present() -> bool {
    SELECTED_SOURCE
        .lock()
        .as_ref()
        .is_some_and(|source| !source.collector.state.lock().is_failed)
}

pub(crate) fn is_disabled() -> bool {
    SELECTED_SOURCE
        .lock()
        .as_ref()
        .is_some_and(|source| source.collector.state.lock().is_failed)
}

// Subscribe before enumerating to avoid missing newly published devices.
pub(crate) fn register_sources() {
    fn note_device(device: ClassDevice<HwrngDeviceImpl>) {
        if !is_virtio_rng(device.name(), device.driver_name()) {
            return;
        }
        {
            let slot = SELECTED_SOURCE.lock();
            if let Some(source) = slot.as_ref() {
                // Rediscovery of the same device must not restart a failed instance.
                if source.collector.device.id() == device.id() {
                    return;
                }
                // Keep the selected healthy source; only a failed source may be replaced.
                if !source.collector.state.lock().is_failed {
                    return;
                }
            }
        }
        let source = match VirtioEntropySource::new(device) {
            Ok(source) => source,
            Err(error) => {
                log::warn!("entropy: failed to prepare VirtIO entropy work: {error:?}");
                return;
            }
        };
        let mut slot = SELECTED_SOURCE.lock();
        if slot
            .as_ref()
            .is_none_or(|current: &Arc<VirtioEntropySource>| {
                current.collector.state.lock().is_failed
            })
        {
            *slot = Some(source);
        }
    }
    subscribe_hwrng_available(Arc::new(note_device));
    for device in hwrng_devices() {
        note_device(device);
    }
}

// Release the registry lock before queuing collection work.
pub(crate) fn request_entropy() {
    let source = SELECTED_SOURCE.lock().clone();
    if let Some(source) = source {
        source.request();
    }
}

fn is_virtio_rng(name: &str, driver_name: &str) -> bool {
    driver_name.contains("virtio-rng") || name == "virtio-rng"
}

#[cfg(unittest)]
mod real_device_tests;

#[unittest::mod_test]
mod tests {
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
}
