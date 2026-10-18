// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::{alloc::Layout, sync::atomic::Ordering};

use device_res::CpuBuffer;
use unittest::def_test;

use super::*;
use crate::word_io::{provider_fixture::Provider, test_provider};

fn unaligned(bytes: &mut [u8], len: usize) -> &mut [u8] {
    let start = bytes.as_ptr().align_offset(4) + 1;
    &mut bytes[start..start + len]
}

#[def_test]
fn sdhci_real_adapter_accepts_unaligned_multisector_read_and_write() {
    static PROVIDER: Provider = Provider::new();
    let allocated = PROVIDER.allocations.load(Ordering::Relaxed);
    let freed = PROVIDER.frees.load(Ordering::Relaxed);
    let device = SDHCIDriver::new(&PROVIDER).unwrap();
    device.inner.lock().storage[..BLOCK_SIZE / 4].fill(0x73737373);
    device.inner.lock().storage[BLOCK_SIZE / 4..2 * BLOCK_SIZE / 4].fill(0x42424242);
    let mut storage = [0xa5; 2 * BLOCK_SIZE + 8];
    let output = unaligned(&mut storage, 2 * BLOCK_SIZE);
    device.read_block(0, output).unwrap();
    assert_eq!(&output[..BLOCK_SIZE], &[0x73; BLOCK_SIZE]);
    assert_eq!(&output[BLOCK_SIZE..], &[0x42; BLOCK_SIZE]);
    assert_eq!(device.inner.lock().reads, 2);
    assert_eq!(PROVIDER.allocations.load(Ordering::Relaxed), allocated + 1);
    assert_eq!(PROVIDER.frees.load(Ordering::Relaxed), freed + 1);
    output.fill(0x59);
    device.write_block(2, output).unwrap();
    assert!(
        device.inner.lock().storage[2 * BLOCK_SIZE / 4..4 * BLOCK_SIZE / 4]
            .iter()
            .all(|&word| word == 0x59595959)
    );
    assert_eq!(device.inner.lock().writes, 2);
    assert_eq!(PROVIDER.allocations.load(Ordering::Relaxed), allocated + 2);
    assert_eq!(PROVIDER.frees.load(Ordering::Relaxed), freed + 2);
    assert_eq!(PROVIDER.last_size.load(Ordering::Relaxed), 2 * BLOCK_SIZE);
}

#[def_test]
fn sdhci_real_adapter_forwards_aligned_and_rejects_invalid_geometry_before_io() {
    static FAIL: Provider = Provider::with_failure(true);
    let mut device = SDHCIDriver::new(&FAIL).unwrap();
    let mut buffer = CpuBuffer::alloc_with(
        test_provider(),
        Layout::from_size_align(BLOCK_SIZE, 4).unwrap(),
    )
    .unwrap();
    let output = buffer.as_mut_slice();
    let pointer = output.as_ptr() as usize;
    device.read_block(0, output).unwrap();
    assert_eq!(device.inner.lock().last_read_address, pointer);
    device.write_block(0, output).unwrap();
    assert_eq!(device.inner.lock().last_write_address, pointer);
    device.block_size = BLOCK_SIZE * 2;
    assert_eq!(device.read_block(0, output), Err(DriverError::Unsupported));
    assert_eq!(device.write_block(0, output), Err(DriverError::Unsupported));
    device.block_size = BLOCK_SIZE;
    assert_eq!(
        device.read_block(0, &mut output[..511]),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(
        device.write_block(0, &output[..511]),
        Err(DriverError::InvalidInput)
    );
    assert_eq!(device.read_block(8, output), Err(DriverError::InvalidInput));
    assert_eq!(
        device.write_block(u64::MAX, output),
        Err(DriverError::InvalidInput)
    );
    device.read_block(8, &mut []).unwrap();
    device.write_block(8, &[]).unwrap();
    assert_eq!(device.inner.lock().reads, 1);
    assert_eq!(device.inner.lock().writes, 1);
    assert_eq!(FAIL.allocations.load(Ordering::Relaxed), 0);
    assert_eq!(FAIL.frees.load(Ordering::Relaxed), 0);
}

#[def_test]
fn sdhci_real_adapter_preserves_errors_releases_storage_and_never_retries() {
    static PROVIDER: Provider = Provider::new();
    let allocated = PROVIDER.allocations.load(Ordering::Relaxed);
    let freed = PROVIDER.frees.load(Ordering::Relaxed);
    let device = SDHCIDriver::new(&PROVIDER).unwrap();
    device.inner.lock().storage.fill(0x73737373);
    device.inner.lock().read_failure = Some((1, SDHCIError::Io));
    device.inner.lock().write_failure = Some((1, SDHCIError::Again));
    let mut storage = [0xa5; 2 * BLOCK_SIZE + 8];
    let output = unaligned(&mut storage, 2 * BLOCK_SIZE);
    assert_eq!(device.read_block(0, output), Err(DriverError::Io));
    assert!(output.iter().all(|&byte| byte == 0xa5));
    assert_eq!(device.inner.lock().reads, 2);
    assert_eq!(PROVIDER.frees.load(Ordering::Relaxed), freed + 1);
    assert_eq!(device.write_block(0, output), Err(DriverError::WouldBlock));
    assert_eq!(device.inner.lock().writes, 2);
    assert!(
        device.inner.lock().storage[..BLOCK_SIZE / 4]
            .iter()
            .all(|&word| word == 0xa5a5a5a5)
    );
    assert!(
        device.inner.lock().storage[BLOCK_SIZE / 4..]
            .iter()
            .all(|&word| word == 0x73737373)
    );
    assert_eq!(PROVIDER.allocations.load(Ordering::Relaxed), allocated + 2);
    assert_eq!(PROVIDER.frees.load(Ordering::Relaxed), freed + 2);
}

#[def_test]
fn sdhci_real_adapter_preserves_all_sdk_error_variants() {
    static PROVIDER: Provider = Provider::new();
    for (source, expected) in [
        (SDHCIError::Io, DriverError::Io),
        (SDHCIError::AlreadyExists, DriverError::AlreadyExists),
        (SDHCIError::Again, DriverError::WouldBlock),
        (SDHCIError::BadState, DriverError::BadState),
        (SDHCIError::InvalidParam, DriverError::InvalidInput),
        (SDHCIError::NoMemory, DriverError::NoMemory),
        (SDHCIError::ResourceBusy, DriverError::ResourceBusy),
        (SDHCIError::Unsupported, DriverError::Unsupported),
    ] {
        let allocated = PROVIDER.allocations.load(Ordering::Relaxed);
        let freed = PROVIDER.frees.load(Ordering::Relaxed);
        let device = SDHCIDriver::new(&PROVIDER).unwrap();
        device.inner.lock().read_failure = Some((0, source));
        device.inner.lock().write_failure = Some((0, source));
        let mut storage = [0xa5; BLOCK_SIZE + 8];
        let output = unaligned(&mut storage, BLOCK_SIZE);
        assert_eq!(device.read_block(0, output), Err(expected));
        assert_eq!(device.inner.lock().reads, 1);
        assert!(output.iter().all(|&byte| byte == 0xa5));
        assert_eq!(device.write_block(0, output), Err(expected));
        assert_eq!(device.inner.lock().writes, 1);
        assert!(device.inner.lock().storage.iter().all(|&word| word == 0));
        assert_eq!(PROVIDER.allocations.load(Ordering::Relaxed), allocated + 2);
        assert_eq!(PROVIDER.frees.load(Ordering::Relaxed), freed + 2);
    }
}
