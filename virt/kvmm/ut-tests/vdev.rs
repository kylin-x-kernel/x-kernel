// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VirtIO block driver-sequence regression through the reusable device self-test.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicUsize, Ordering};

use kalloc_types::{AllocFlags, KBox};
use unittest::{assert, assert_eq, def_test};
use vdev_virtio_blk::selftest::vmm_selftest_virtio_blk;

use super::{MmioBus, MmioDevice};

struct OwnedDevice {
    value: u64,
    drops: Arc<AtomicUsize>,
}

impl Drop for OwnedDevice {
    fn drop(&mut self) {
        self.drops.fetch_add(1, Ordering::Relaxed);
    }
}

impl MmioDevice for OwnedDevice {
    fn name(&self) -> &str {
        "owned-test"
    }

    fn mmio_range(&self) -> (u64, u64) {
        (0x1000, 8)
    }

    fn read(&self, _offset: u64, _size: u8) -> u64 {
        self.value
    }

    fn write(&mut self, _offset: u64, _size: u8, value: u64) {
        self.value = value;
    }
}

#[def_test]
fn prepared_mmio_owner_dispatches_and_drops_once() {
    let drops = Arc::new(AtomicUsize::new(0));
    let owner = KBox::new_nowait(OwnedDevice { value: 7, drops: drops.clone() }).unwrap();
    let mut bus = MmioBus::new();
    bus.register(owner);
    assert_eq!(bus.handle(0x1000, false, 8, 0), Some(7));
    assert_eq!(bus.handle(0x1000, true, 8, 19), Some(0));
    assert_eq!(bus.handle(0x1000, false, 8, 0), Some(19));
    assert_eq!(drops.load(Ordering::Relaxed), 0);
    drop(bus);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

#[def_test]
fn rejected_mmio_owner_is_not_registered() {
    let drops = Arc::new(AtomicUsize::new(0));
    let mut bus = MmioBus::new();
    let owner = KBox::new_with_flags(
        OwnedDevice { value: 7, drops: drops.clone() },
        AllocFlags::from_bits_retain(u32::MAX),
    );
    assert!(owner.is_err());
    assert_eq!(bus.handle(0x1000, false, 8, 0), None);
    assert_eq!(drops.load(Ordering::Relaxed), 1);
}

/// Full virtio-blk driver sequence against the vdev-virtio-blk device: negotiation,
/// queue programming, OUT/IN/FLUSH data path, used-ring accounting and
/// interrupt injection.
#[def_test(serial)]
fn test_virtio_blk_device_driver_sequence() {
    assert!(vmm_selftest_virtio_blk());
}

#[cfg(target_arch = "aarch64")]
#[def_test]
fn gicd_initializes_caller_owned_register_storage() {
    use super::{VcpuWaker, aarch64::{vgic::Vgic, vgicd::{Vgicd, REG_SIZE}}};

    struct NoopWaker;
    impl VcpuWaker for NoopWaker {
        fn wake_vcpu(&self, _vcpu_id: u32) {}
    }

    let waker: Arc<dyn VcpuWaker> = Arc::new(NoopWaker);
    let vgic = Vgic::new(1, 0, Arc::downgrade(&waker));
    let registers = KBox::new_nowait([0xff; REG_SIZE]).unwrap();
    let mut distributor = Vgicd::new(vgic, 1, registers);
    assert_eq!(distributor.read(0xc00, 4), 0);
    distributor.write(0xc00, 4, 3);
    assert_eq!(distributor.read(0xc00, 4), 3);
}
