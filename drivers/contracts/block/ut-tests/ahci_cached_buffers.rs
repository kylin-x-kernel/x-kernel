// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{alloc::Layout, sync::atomic::Ordering};

use device_res::CpuBuffer;
use unittest::def_test;

use super::*;
use crate::word_io::{provider_fixture::Provider, test_provider};

struct CoherentHal;

impl AhciControllerHal for CoherentHal {
    fn virt_to_phys(address: usize) -> usize {
        address
    }

    fn current_ms() -> u64 {
        0
    }

    fn flush_dcache() {}
}

// SAFETY: the UT controller copies bytes on the CPU; there is no device or
// separate cache domain. This claim is confined to the CPU fixture.
unsafe impl AhciHal for CoherentHal {
    const DMA_COHERENT: bool = true;
}

struct NonCoherentHal;

impl AhciControllerHal for NonCoherentHal {
    fn virt_to_phys(_address: usize) -> usize {
        panic!("non-coherent HAL must be rejected before SDK entry")
    }

    fn current_ms() -> u64 {
        panic!("non-coherent HAL must not initialize the SDK")
    }

    fn flush_dcache() {
        panic!("an ordering barrier cannot certify cache coherency")
    }
}

// SAFETY: no coherent DMA capability is claimed. All controller callbacks are
// sentinels, so reaching one makes the admission regression fail deterministically.
unsafe impl AhciHal for NonCoherentHal {
    const DMA_COHERENT: bool = false;
}

fn device(provider: &'static dyn CpuMemoryOp) -> AhciDriver<CoherentHal> {
    let mut registers = [0_u64; 8];
    // SAFETY: cfg(unittest) replaces the SDK with the CPU controller, which
    // retains no MMIO borrow and only observes this live, aligned backing's
    // address during construction. No hardware access or DMA occurs.
    unsafe { AhciDriver::new(registers.as_mut_ptr() as usize, provider) }.unwrap()
}

#[def_test]
fn ahci_rejects_noncoherent_hal_before_sdk_or_cpu_allocation() {
    static PROVIDER: Provider = Provider::with_failure(true);
    let mut registers = [0_u64; 8];
    // SAFETY: the UT controller has no MMIO/DMA; live aligned backing is supplied
    // anyway. The false capability must return before any SDK/HAL callback.
    let result =
        unsafe { AhciDriver::<NonCoherentHal>::new(registers.as_mut_ptr() as usize, &PROVIDER) };
    assert!(result.is_none());
    assert_eq!(PROVIDER.allocations.load(Ordering::Relaxed), 0);
    assert_eq!(PROVIDER.frees.load(Ordering::Relaxed), 0);
}

#[def_test]
fn ahci_coherent_adapter_keeps_aligned_io_allocation_free_and_stages_unaligned_io() {
    static PROVIDER: Provider = Provider::new();
    let allocated = PROVIDER.allocations.load(Ordering::Relaxed);
    let freed = PROVIDER.frees.load(Ordering::Relaxed);
    let disk = device(&PROVIDER);
    let mut aligned =
        CpuBuffer::alloc_with(test_provider(), Layout::from_size_align(512, 4).unwrap()).unwrap();
    let bytes = aligned.as_mut_slice();
    let pointer = bytes.as_ptr() as usize;
    disk.read_block(0, bytes).unwrap();
    assert!(bytes.iter().all(|&b| b == 0x73));
    disk.write_block(1, bytes).unwrap();
    assert_eq!(disk.inner.lock().last_read_address, pointer);
    assert_eq!(disk.inner.lock().last_write_address, pointer);
    assert_eq!(PROVIDER.allocations.load(Ordering::Relaxed), allocated);
    assert_eq!(PROVIDER.frees.load(Ordering::Relaxed), freed);

    let mut storage = [0xa5; 1032];
    let start = storage.as_ptr().align_offset(4) + 1;
    let bytes = &mut storage[start..start + 1024];
    disk.read_block(0, bytes).unwrap();
    assert!(bytes.iter().all(|&b| b == 0x73));
    bytes.fill(0x42);
    disk.write_block(2, bytes).unwrap();
    assert!(
        disk.inner.lock().storage[1024..2048]
            .iter()
            .all(|&b| b == 0x42)
    );
    assert_eq!(PROVIDER.allocations.load(Ordering::Relaxed), allocated + 2);
    assert_eq!(PROVIDER.frees.load(Ordering::Relaxed), freed + 2);
    assert_ne!(disk.inner.lock().last_read_address, bytes.as_ptr() as usize);
    assert!(disk.inner.lock().last_read_address.is_multiple_of(4));
}

#[def_test]
fn ahci_coherent_adapter_returns_allocation_and_io_errors_without_retries() {
    static PROVIDER: Provider = Provider::new();
    static FAIL: Provider = Provider::with_failure(true);
    let allocated = PROVIDER.allocations.load(Ordering::Relaxed);
    let freed = PROVIDER.frees.load(Ordering::Relaxed);
    let disk = device(&PROVIDER);
    disk.inner.lock().fail_read = true;
    disk.inner.lock().fail_write = true;
    let mut storage = [0xa5; 520];
    let start = storage.as_ptr().align_offset(4) + 1;
    let bytes = &mut storage[start..start + 512];
    assert_eq!(disk.read_block(0, bytes), Err(DriverError::Io));
    assert!(bytes.iter().all(|&b| b == 0xa5));
    assert_eq!(disk.write_block(0, bytes), Err(DriverError::Io));
    assert_eq!(disk.inner.lock().reads, 1);
    assert_eq!(disk.inner.lock().writes, 1);
    assert_eq!(PROVIDER.allocations.load(Ordering::Relaxed), allocated + 2);
    assert_eq!(PROVIDER.frees.load(Ordering::Relaxed), freed + 2);

    let disk = device(&FAIL);
    assert_eq!(disk.read_block(0, bytes), Err(DriverError::NoMemory));
    assert_eq!(disk.write_block(0, bytes), Err(DriverError::NoMemory));
    assert_eq!(disk.inner.lock().reads, 0);
    assert_eq!(disk.inner.lock().writes, 0);
    assert_eq!(FAIL.allocations.load(Ordering::Relaxed), 2);
    assert_eq!(FAIL.frees.load(Ordering::Relaxed), 0);
}
