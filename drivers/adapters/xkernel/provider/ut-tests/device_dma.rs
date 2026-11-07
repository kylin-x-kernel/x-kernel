// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ptr::NonNull;

use driver_os_api::{DmaInfo, DmaSpec, DmaWindow, ResError};
use khal::mem::v2p;
use memaddr::VirtAddr;
use unittest::{assert, assert_eq, def_test};

use super::*;

fn identity() -> DmaInfo {
    DmaInfo::new(u64::MAX, DmaWindow::IDENTITY, true).unwrap()
}

#[def_test]
fn noncoherent_devices_are_rejected_instead_of_defaulted() {
    let info = DmaInfo::new(u64::MAX, DmaWindow::IDENTITY, false).unwrap();
    assert_eq!(
        XKernelDeviceDma::new(info).unwrap_err(),
        ResError::Unsupported
    );
}

#[def_test]
fn coherent_allocations_are_translated_and_released_once() {
    let dma = XKernelDeviceDma::new(identity()).unwrap();
    let allocation = dma.alloc_coherent(DmaSpec::new(64, 8)).unwrap();
    let physical = v2p(VirtAddr::from_usize(allocation.cpu_addr)).as_usize() as u64;

    assert_eq!(allocation.bus_addr, physical);
    assert_eq!(allocation.spec.len, 64);
    dma.free_coherent(allocation);
}

#[def_test]
fn coherent_allocations_are_zero_filled() {
    let dma = XKernelDeviceDma::new(identity()).unwrap();

    // Dirty a buffer first so a recycled page cannot pass as "naturally zero".
    let dirty = dma.alloc_coherent(DmaSpec::new(64, 8)).unwrap();
    // SAFETY: `dirty` is a live, exclusive allocation of at least 64 bytes.
    unsafe { core::ptr::write_bytes(dirty.cpu_addr as *mut u8, 0xaa, 64) };
    dma.free_coherent(dirty);

    let allocation = dma.alloc_coherent(DmaSpec::new(64, 8)).unwrap();
    // SAFETY: the allocation is live and the requested extent is readable.
    let bytes = unsafe { core::slice::from_raw_parts(allocation.cpu_addr as *const u8, 64) };
    assert!(bytes.iter().all(|byte| *byte == 0));
    dma.free_coherent(allocation);
}

#[def_test]
fn allocations_outside_the_window_are_refused() {
    let info = DmaInfo::new(u64::MAX, DmaWindow::new(0, 0, 0x1000).unwrap(), true).unwrap();
    let dma = XKernelDeviceDma::new(info).unwrap();

    assert_eq!(
        dma.alloc_coherent(DmaSpec::new(64, 8)).unwrap_err(),
        ResError::NoMemory
    );
}

#[def_test]
fn streaming_mappings_translate_and_release() {
    let dma = XKernelDeviceDma::new(identity()).unwrap();
    let mut buffer = [0u8; 64];
    let pointer = NonNull::from(&mut buffer[..]);

    // SAFETY: the stack buffer is initialized, exclusively borrowed for the
    // mapping lifetime, and no device accesses the returned address.
    let mapping = unsafe { dma.map_streaming(pointer, driver_os_api::DmaDirection::Bidirectional) }
        .unwrap();
    assert_eq!(mapping.len(), buffer.len());
    assert!(mapping.bus_addr() != 0);
    drop(mapping);
}

/// Rollback runs while the mapping is still unpublished, so a failed
/// DeviceToDriver mapping must not copy anything back over the caller's data.
#[def_test]
fn failed_device_to_driver_mapping_preserves_the_buffer() {
    let info = DmaInfo::new(u64::MAX, DmaWindow::new(0, 0, 0x1000).unwrap(), true).unwrap();
    let dma = XKernelDeviceDma::new(info).unwrap();
    let mut buffer = [0xa5u8; 64];
    let pointer = NonNull::from(&mut buffer[..]);

    // The bounce allocation lies outside the window, so the mapping is
    // rejected before any device can receive its address.
    // SAFETY: the stack buffer is initialized and exclusively borrowed; the
    // provider rejects the mapping before publishing it.
    let error =
        unsafe { dma.map_streaming(pointer, driver_os_api::DmaDirection::DeviceToDriver) }
            .unwrap_err();
    assert_eq!(error, ResError::NoMemory);
    assert!(buffer.iter().all(|byte| *byte == 0xa5));
}

/// Releasing a DeviceToDriver mapping that the device never wrote must return
/// the original bytes; queue-setup rollback depends on this.
#[def_test]
fn unreleased_device_to_driver_mapping_round_trip_preserves_the_buffer() {
    let dma = XKernelDeviceDma::new(identity()).unwrap();
    let mut buffer = [0x5au8; 64];
    let pointer = NonNull::from(&mut buffer[..]);

    // SAFETY: the stack buffer is initialized, exclusively borrowed, and no
    // device accesses the mapping before it is dropped.
    let mapping =
        unsafe { dma.map_streaming(pointer, driver_os_api::DmaDirection::DeviceToDriver) }
            .unwrap();
    drop(mapping);
    assert!(buffer.iter().all(|byte| *byte == 0x5a));
}
