// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO RNG entropy source (trusts the VMM / Host).

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use char_driver::CharDevice;
use kclass::{CharDeviceImpl, ClassDevice, char_devices, subscribe_char_available};
use klazy::Lazy;
use ksync::Mutex;

static VIRTIO_PRESENT: AtomicBool = AtomicBool::new(false);
/// Set after repeated timed-out / failed reads so reseed stops hammering a
/// wedged VirtIO RNG under `SpinNoIrq`.
static VIRTIO_DISABLED: AtomicBool = AtomicBool::new(false);
static CONSECUTIVE_FAILURES: AtomicU32 = AtomicU32::new(0);
const MAX_CONSECUTIVE_FAILURES: u32 = 3;

/// Cached VirtIO RNG handle so reseed does not re-scan `char_devices()`.
static CACHED_DEVICE: Lazy<Mutex<Option<ClassDevice<CharDeviceImpl>>>> =
    Lazy::new(|| Mutex::new(None));

pub(crate) fn is_present() -> bool {
    VIRTIO_PRESENT.load(Ordering::Acquire) && !VIRTIO_DISABLED.load(Ordering::Acquire)
}

pub(crate) fn register_sources() {
    if !kbuild_config::KFEAT_DRIVER_VIRTIO_RNG {
        return;
    }

    fn note_device(device: &ClassDevice<CharDeviceImpl>) {
        if !is_virtio_rng(device.name(), device.driver_name()) {
            return;
        }

        let mut slot = CACHED_DEVICE.lock();
        if slot.is_some() {
            return;
        }

        *slot = Some(device.clone());
        VIRTIO_PRESENT.store(true, Ordering::Release);
        VIRTIO_DISABLED.store(false, Ordering::Release);
        CONSECUTIVE_FAILURES.store(0, Ordering::Release);
        log::info!(
            "entropy: VirtIO RNG discovered ({}, driver={})",
            device.name(),
            device.driver_name()
        );
    }

    for device in char_devices() {
        note_device(&device);
    }

    subscribe_char_available(Arc::new(|device| note_device(&device)));
}

pub(crate) fn read(len: usize) -> Option<Vec<u8>> {
    if !kbuild_config::KFEAT_DRIVER_VIRTIO_RNG || !is_present() {
        return None;
    }

    // Clone the Arc handle under the lock, then release before device I/O
    // (VirtIO RNG may poll the virtqueue under SpinNoIrq with a deadline).
    let device = CACHED_DEVICE.lock().clone()?;
    read_from_device(&device, len)
}

fn read_from_device(device: &ClassDevice<CharDeviceImpl>, len: usize) -> Option<Vec<u8>> {
    let mut buf = alloc::vec![0u8; len];
    match device.read(&mut buf) {
        Ok(read) if read > 0 => {
            CONSECUTIVE_FAILURES.store(0, Ordering::Release);
            buf.truncate(read);
            Some(buf)
        }
        _ => {
            note_read_failure();
            None
        }
    }
}

fn note_read_failure() {
    let failures = CONSECUTIVE_FAILURES.fetch_add(1, Ordering::AcqRel) + 1;
    if failures >= MAX_CONSECUTIVE_FAILURES && !VIRTIO_DISABLED.swap(true, Ordering::AcqRel) {
        log::warn!(
            "entropy: disabling VirtIO RNG after {failures} consecutive read failures/timeouts"
        );
    }
}

fn is_virtio_rng(name: &str, driver_name: &str) -> bool {
    driver_name.contains("virtio-rng") || name == "virtio-rng"
}

#[cfg(unittest)]
mod tests {
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

    #[def_test]
    fn test_read_without_device_returns_none() {
        if !is_present() {
            assert!(read(32).is_none());
        }
    }

    #[def_test]
    fn test_read_disabled_without_kconfig() {
        if !kbuild_config::KFEAT_DRIVER_VIRTIO_RNG {
            assert!(!is_present());
            assert!(read(16).is_none());
        }
    }

    #[def_test]
    fn test_read_when_present() {
        if kbuild_config::KFEAT_DRIVER_VIRTIO_RNG
            && is_present()
            && let Some(data) = read(32)
        {
            assert_eq!(data.len(), 32);
            assert!(data.iter().any(|&b| b != 0));
        }
    }
}
