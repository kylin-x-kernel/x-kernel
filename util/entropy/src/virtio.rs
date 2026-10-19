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

unittest::path_mod_test!(real_device_tests, "../ut-tests/virtio_real_device_tests.rs");

unittest::path_mod_test!(tests, "../ut-tests/virtio.rs");
