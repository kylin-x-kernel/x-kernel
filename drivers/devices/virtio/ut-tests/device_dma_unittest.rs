// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};

use driver_os_api::{
    DeviceDma, DmaAllocation, DmaDirection, DmaInfo, DmaSpec, DmaWindow, ReleaseHook, ResError,
    ResResult, StreamingMapping,
};
use unittest::{assert_eq, def_test};
use virtio_drivers::{BufferDirection, PAGE_SIZE};

use super::*;

const BASE: usize = 0x1000_0000;

struct FakeDma {
    next: AtomicUsize,
    allocations: AtomicUsize,
    frees: AtomicUsize,
    mappings: AtomicUsize,
    unmaps: Arc<AtomicUsize>,
}

impl FakeDma {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            next: AtomicUsize::new(0),
            allocations: AtomicUsize::new(0),
            frees: AtomicUsize::new(0),
            mappings: AtomicUsize::new(0),
            unmaps: Arc::new(AtomicUsize::new(0)),
        })
    }

    fn unmaps(&self) -> usize {
        self.unmaps.load(Ordering::Relaxed)
    }
}

impl DeviceDma for FakeDma {
    fn info(&self) -> DmaInfo {
        DmaInfo::new(u64::MAX, DmaWindow::IDENTITY, true).unwrap()
    }

    fn alloc_coherent(&self, spec: DmaSpec) -> ResResult<DmaAllocation> {
        self.allocations.fetch_add(1, Ordering::Relaxed);
        let offset = self
            .next
            .fetch_add(spec.len.max(PAGE_SIZE), Ordering::Relaxed);
        let cpu_addr = BASE + offset;
        Ok(DmaAllocation {
            cpu_addr,
            bus_addr: cpu_addr as u64,
            spec,
        })
    }

    fn free_coherent(&self, _alloc: DmaAllocation) {
        self.frees.fetch_add(1, Ordering::Relaxed);
    }

    unsafe fn map_streaming(
        &self,
        buffer: NonNull<[u8]>,
        _direction: DmaDirection,
    ) -> ResResult<StreamingMapping> {
        if buffer.is_empty() {
            return Err(ResError::InvalidResource);
        }
        self.mappings.fetch_add(1, Ordering::Relaxed);
        let len = buffer.len();
        let bus_addr = buffer.as_ptr() as *mut u8 as usize as u64;
        let unmaps = self.unmaps.clone();
        let release: ReleaseHook = Arc::new(move || {
            unmaps.fetch_add(1, Ordering::Relaxed);
        });
        // SAFETY: the guard describes the interval just returned and `release`
        // runs once, only counting the unmap.
        Ok(unsafe { StreamingMapping::new(bus_addr, len, release) })
    }
}

fn backend(fake: &Arc<FakeDma>) -> DeviceDmaBackend {
    let capability: Arc<dyn DeviceDma> = fake.clone();
    // SAFETY: the fake is synchronous, non-sleeping, exclusive to this test and
    // identity-mapped; release only increments a counter.
    unsafe { DeviceDmaBackend::new(capability) }
}

#[def_test]
fn allocation_round_trip_releases_the_exact_tuple() {
    let fake = FakeDma::new();
    let backend = backend(&fake);

    let (bus, pointer) = backend.allocate(2, BufferDirection::Both, false).unwrap();
    assert_eq!(bus as usize, pointer.as_ptr() as usize);
    assert_eq!(bus as usize % PAGE_SIZE, 0);
    assert_eq!(fake.allocations.load(Ordering::Relaxed), 1);

    // SAFETY: this is the live tuple returned above, released once.
    unsafe { backend.deallocate(bus, pointer, 2, BufferDirection::Both, false) };
    assert_eq!(fake.frees.load(Ordering::Relaxed), 1);
}

#[def_test]
fn streaming_mapping_is_released_exactly_once() {
    let fake = FakeDma::new();
    let backend = backend(&fake);
    let mut bytes = [0u8; 8];
    let buffer = NonNull::slice_from_raw_parts(
        NonNull::new(bytes.as_mut_ptr()).unwrap(),
        bytes.len(),
    );

    // SAFETY: the local array stays live and exclusively held until release.
    let bus = unsafe { backend.share(buffer, BufferDirection::DeviceToDriver, true) }.unwrap();
    assert_eq!(fake.mappings.load(Ordering::Relaxed), 1);
    assert_eq!(fake.unmaps(), 0);

    // SAFETY: exact tuple from the successful share above; there is no device.
    unsafe { backend.unshare(bus, buffer, BufferDirection::DeviceToDriver, true) };
    assert_eq!(fake.unmaps(), 1);
}

#[def_test]
fn a_duplicate_live_mapping_is_rejected_and_its_token_released() {
    let fake = FakeDma::new();
    let backend = backend(&fake);
    let mut bytes = [0u8; 8];
    let buffer = NonNull::slice_from_raw_parts(
        NonNull::new(bytes.as_mut_ptr()).unwrap(),
        bytes.len(),
    );

    // SAFETY: the local array stays live and exclusively held until release.
    let bus = unsafe { backend.share(buffer, BufferDirection::DriverToDevice, false) }.unwrap();
    assert_eq!(fake.mappings.load(Ordering::Relaxed), 1);

    // The fake returns the same bus address for the same buffer, so the second
    // record collides with the live one and must be released immediately.
    // SAFETY: the buffer is still live; the provider maps it twice, and the
    // rejected token is dropped by the adapter.
    let duplicate = unsafe { backend.share(buffer, BufferDirection::DriverToDevice, false) };
    assert_eq!(duplicate, Err(virtio_drivers::Error::AlreadyUsed));
    assert_eq!(fake.mappings.load(Ordering::Relaxed), 2);
    assert_eq!(fake.unmaps(), 1);

    // SAFETY: exact tuple from the accepted share above.
    unsafe { backend.unshare(bus, buffer, BufferDirection::DriverToDevice, false) };
    assert_eq!(fake.unmaps(), 2);
}

#[def_test]
fn empty_buffer_is_rejected_before_the_provider() {
    let fake = FakeDma::new();
    let backend = backend(&fake);
    let mut byte = [0u8; 1];
    let buffer = NonNull::slice_from_raw_parts(
        NonNull::new(byte.as_mut_ptr()).unwrap(),
        0,
    );

    // SAFETY: an empty buffer must be rejected before any provider call.
    let result = unsafe { backend.share(buffer, BufferDirection::DriverToDevice, false) };
    assert_eq!(result, Err(virtio_drivers::Error::InvalidParam));
    assert_eq!(fake.mappings.load(Ordering::Relaxed), 0);
}
