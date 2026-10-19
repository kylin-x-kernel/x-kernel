// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Integration tests against a published, real VirtIO RNG device.
//!
//! QEMU attaches the RNG device whenever `KFEAT_DRIVER_VIRTIO_RNG` is enabled.
//! These tests borrow its boot-owned handle; they neither register devices nor
//! reset the entropy pool.

use unittest::{TestResult, assert, assert_eq, def_test};

use super::*;

fn find_device() -> Option<ClassDevice<HwrngDeviceImpl>> {
    hwrng_devices()
        .into_iter()
        .find(|device| is_virtio_rng(device.name(), device.driver_name()))
}

#[def_test(serial)]
fn test_empty_read() {
    let device = find_device();
    assert!(
        device.is_some(),
        "QEMU must publish a real virtio-rng device"
    );
    let device = device.unwrap();
    assert_eq!(device.read(&mut []), Ok(0));
}

#[def_test(serial)]
fn test_read_lengths_and_buffer_boundaries() {
    let device = find_device();
    assert!(
        device.is_some(),
        "QEMU must publish a real virtio-rng device"
    );
    let device = device.unwrap();

    // The cap is the current X-Kernel adapter contract, not a VirtIO limit.
    const MAX_READ_BYTES: usize = 256;
    const GUARD_BYTES: usize = 16;
    const SENTINEL: u8 = 0xa5;
    for len in [1usize, 32, 64, 256, 257, 1024] {
        let mut storage = alloc::vec![SENTINEL; len + 2 * GUARD_BYTES];
        let mut filled = 0;
        // Each successful request must make progress, so at most len calls
        // suffice even when the device returns one byte per request.
        for _ in 0..len {
            let remaining = len - filled;
            let result = device.read(&mut storage[GUARD_BYTES + filled..GUARD_BYTES + len]);
            let read = match result {
                Ok(read) => read,
                Err(error) => {
                    unittest::ktest_println!("RNG read len={len}, offset={filled}: {error:?}");
                    return TestResult::Failed;
                }
            };
            assert!(read > 0 && read <= remaining.min(MAX_READ_BYTES));
            assert!(storage[..GUARD_BYTES].iter().all(|byte| *byte == SENTINEL));
            assert!(
                storage[GUARD_BYTES + len..]
                    .iter()
                    .all(|byte| *byte == SENTINEL)
            );
            // A short read must not modify the unreported suffix either.
            assert!(
                storage[GUARD_BYTES + filled + read..GUARD_BYTES + len]
                    .iter()
                    .all(|byte| *byte == SENTINEL)
            );
            filled += read;
            if filled == len {
                break;
            }
        }
        assert_eq!(filled, len);
    }
}

#[def_test(serial)]
fn test_repeated_reads_recycle_queue() {
    let device = find_device();
    assert!(
        device.is_some(),
        "QEMU must publish a real virtio-rng device"
    );
    let device = device.unwrap();
    let mut buf = [0u8; 64];
    // Exercise reuse well beyond the adapter's eight queue entries without
    // assuming that random samples are nonzero or mutually different.
    for request in 0..128 {
        let result = device.read(&mut buf);
        let read = match result {
            Ok(read) => read,
            Err(error) => {
                unittest::ktest_println!("RNG request {request}: {error:?}");
                return TestResult::Failed;
            }
        };
        assert!(read > 0 && read <= buf.len());
    }
}
