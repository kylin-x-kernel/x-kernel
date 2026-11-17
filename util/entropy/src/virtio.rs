// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO demand-driven entropy collection (requires Host trust).

use alloc::sync::Arc;

use hwrng::HwrngDevice;
use kalloc_types::KBox;
use kclass::{
    ClassDevice, HwrngDeviceImpl, hwrng_devices, subscribe_hwrng_available, subscribe_hwrng_removed,
};
use kdevice::PublicationId;
use kerrno::{KError, KResult};
use klazy::Lazy;
use kspin::SpinNoIrq;
use ksync::Mutex;
use kworkng::{HandleCreateError, SubmitError, WorkHandle};

// Back off failed reads, not in-flight device completion waits.
const READ_RETRY_DELAY: ktime_types::TimeSpan = ktime_types::TimeSpan::from_millis(100);

static SELECTED_SOURCE: Lazy<Mutex<Option<Arc<VirtioEntropySource>>>> =
    Lazy::new(|| Mutex::new(None));

#[cfg(unittest)]
type RetryHook = Arc<dyn Fn() + Send + Sync>;

#[cfg(unittest)]
static RETRY_PAUSE: Lazy<Mutex<Option<RetryHook>>> = Lazy::new(|| Mutex::new(None));

#[cfg(unittest)]
fn pause_before_retry() {
    let hook = RETRY_PAUSE.lock().clone();
    if let Some(hook) = hook {
        hook();
    }
}

type CollectorWork = WorkHandle<KBox<dyn Fn() + Send + Sync>>;

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
    /// The publication was revoked; late worker results must not be mixed.
    is_revoked: bool,
}

impl VirtioEntropySource {
    /// Prepares a collector in sleepable, reclaim-safe task context with IRQs
    /// and preemption enabled, outside non-sleepable RCU sections. Source,
    /// collector and class-registry locks must not enclose this call. The KERNEL
    /// callback allocation precedes publication; failure retains the old source.
    /// Nested Arc and worker allocations keep their existing failure behavior.
    fn new(device: ClassDevice<HwrngDeviceImpl>) -> KResult<Arc<Self>> {
        if !ktask::is_sleepable_context() {
            return Err(KError::InvalidInput);
        }
        let collector = Arc::new(EntropyCollector {
            device,
            state: SpinNoIrq::new(CollectorState {
                has_demand: false,
                is_failed: false,
                is_revoked: false,
            }),
        });
        let callback = collector.clone();
        let callback: KBox<dyn Fn() + Send + Sync> = KBox::new(move || callback.run())?;
        let work = WorkHandle::new(callback).map_err(|error| match error {
            HandleCreateError::InvalidContext => KError::InvalidInput,
            HandleCreateError::ServicesUnavailable => KError::OperationNotSupported,
        })?;
        Ok(Arc::new(Self { collector, work }))
    }

    fn publication_id(&self) -> PublicationId {
        self.collector.device.publication_id()
    }

    fn stop(&self) {
        self.collector.stop();
        if let Err(error) = self.work.shutdown() {
            log::warn!("entropy: failed to stop VirtIO entropy worker: {error:?}");
        }
    }

    fn request(&self) {
        if !self.collector.device.is_available() {
            return;
        }
        let mut state = self.collector.state.lock();
        if state.is_failed || state.is_revoked {
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

    fn stop(&self) {
        let mut state = self.state.lock();
        state.is_revoked = true;
        state.has_demand = false;
    }

    fn is_failed(&self) -> bool {
        let state = self.state.lock();
        state.is_failed || state.is_revoked
    }

    fn run(&self) {
        {
            let mut state = self.state.lock();
            if state.is_failed || state.is_revoked || !state.has_demand {
                return;
            }
            state.has_demand = false;
        }
        if !self.device.is_available() {
            self.stop();
            return;
        }
        let mut bytes = [0; super::HW_RESEED_BYTES];
        let mut has_reported_error = false;
        // Retain this demand across errors: a getrandom caller may already
        // be asleep and cannot be relied on to request another sample.
        loop {
            {
                let state = self.state.lock();
                if state.is_failed || state.is_revoked || !self.device.is_available() {
                    drop(state);
                    self.stop();
                    return;
                }
            }
            match self.device.read(&mut bytes) {
                Ok(len) if len > 0 && len <= bytes.len() => {
                    if !self.device.is_available() {
                        self.stop();
                        return;
                    }
                    super::mix_virtio_entropy(&bytes[..len]);
                    break;
                }
                Err(error) => {
                    if !self.device.is_available() {
                        self.stop();
                        return;
                    }
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
                    #[cfg(unittest)]
                    pause_before_retry();
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
    SELECTED_SOURCE.lock().as_ref().is_some_and(|source| {
        source.collector.device.is_available() && !source.collector.is_failed()
    })
}

pub(crate) fn is_disabled() -> bool {
    SELECTED_SOURCE
        .lock()
        .as_ref()
        .is_some_and(|source| source.collector.is_failed())
}

fn remove_source(publication: PublicationId) {
    let source = {
        let mut slot = SELECTED_SOURCE.lock();
        if slot
            .as_ref()
            .is_some_and(|current| current.publication_id() == publication)
        {
            slot.take()
        } else {
            None
        }
    };
    if let Some(source) = source {
        source.stop();
    }
}

pub(crate) fn note_device(device: ClassDevice<HwrngDeviceImpl>) {
    if !is_virtio_rng(device.name(), device.driver_name()) {
        return;
    }
    {
        let slot = SELECTED_SOURCE.lock();
        if let Some(source) = slot.as_ref() {
            // Rediscovery of the same publication must not restart it.
            if source.publication_id() == device.publication_id() {
                return;
            }
            // Keep only an available, unfailed source; a revoked publication
            // must be replaceable even before its removal callback runs.
            if source.collector.device.is_available() && !source.collector.is_failed() {
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
    let (previous, rejected) = {
        let mut slot = SELECTED_SOURCE.lock();
        if !source.collector.device.is_available()
            || slot.as_ref().is_some_and(|current| {
                current.publication_id() == source.publication_id()
                    || (current.collector.device.is_available() && !current.collector.is_failed())
            })
        {
            (None, Some(source))
        } else {
            (slot.replace(source), None)
        }
    };
    if let Some(source) = rejected {
        source.stop();
    }
    if let Some(previous) = previous {
        previous.stop();
    }
}

// Subscribe before enumerating to avoid missing newly published devices.
pub(crate) fn register_sources() {
    subscribe_hwrng_removed(Arc::new(|_, publication| remove_source(publication)));
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
