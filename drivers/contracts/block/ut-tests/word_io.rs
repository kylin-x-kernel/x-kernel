// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, string::String, sync::Arc, vec, vec::Vec};
use core::sync::atomic::Ordering;

use unittest::def_test;

use super::{provider_fixture::Provider, *};
use crate::{BlockDeviceOperations, Gendisk, Mutex};

fn unaligned(bytes: &mut [u8], len: usize) -> &mut [u8] {
    let start = bytes.as_ptr().align_offset(align_of::<u32>()) + 1;
    &mut bytes[start..start + len]
}

#[def_test]
fn word_io_aligned_requests_forward_original_buffers_without_allocation() {
    let provider = Provider::with_failure(true);
    let mut owner = staging(test_provider(), 4096).unwrap();
    let output = owner.as_mut_slice();
    let address = output.as_ptr() as usize;
    read_blocks_with_provider(&provider, 2, 512, 10, output, |block, bytes| {
        assert_eq!(block, 2);
        assert_eq!(bytes.as_ptr() as usize, address);
        assert_eq!(bytes.len(), 4096);
        bytes.fill(0x63);
        Ok(())
    })
    .unwrap();
    assert!(output.iter().all(|&byte| byte == 0x63));
    assert_eq!(
        write_blocks_with_provider(&provider, 2, 512, 10, output, |block, bytes| {
            assert_eq!(block, 2);
            assert_eq!(bytes.as_ptr() as usize, address);
            assert_eq!(bytes, &[0x63; 4096]);
            Err(DriverError::ResourceBusy)
        }),
        Err(DriverError::ResourceBusy)
    );
    assert_eq!(provider.allocations.load(Ordering::Relaxed), 0);
    assert_eq!(provider.frees.load(Ordering::Relaxed), 0);
}

#[def_test]
fn word_io_unaligned_reads_and_writes_reuse_one_owned_bounded_buffer() {
    let len = 3 * BOUNCE_BYTES + 512;
    let mut storage = vec![0xa5; len + 8];
    let output = unaligned(&mut storage, len);
    let caller = output.as_ptr() as usize;
    let provider = Provider::new();
    let mut reads = Vec::new();
    read_blocks_with_provider(&provider, 7, 512, 1024, output, |block, bytes| {
        assert_eq!(bytes.as_ptr() as usize % align_of::<u32>(), 0);
        assert_ne!(bytes.as_ptr() as usize, caller);
        reads.push((block, bytes.as_ptr() as usize, bytes.len()));
        for (index, part) in bytes.chunks_mut(512).enumerate() {
            part.fill((block + index as u64) as u8);
        }
        Ok(())
    })
    .unwrap();
    assert_eq!(provider.last_size.load(Ordering::Relaxed), BOUNCE_BYTES);
    assert_eq!(provider.allocations.load(Ordering::Relaxed), 1);
    assert_eq!(provider.frees.load(Ordering::Relaxed), 1);
    assert_eq!(reads.len(), 4);
    assert!(
        reads
            .iter()
            .all(|r| r.1 == reads[0].1 && r.2 <= BOUNCE_BYTES)
    );
    for (index, part) in output.chunks(512).enumerate() {
        assert!(part.iter().all(|&byte| byte == (7 + index as u64) as u8));
    }

    let mut writes = Vec::new();
    write_blocks_with_provider(&provider, 7, 512, 1024, output, |block, bytes| {
        assert_eq!(bytes.as_ptr() as usize % align_of::<u32>(), 0);
        assert_ne!(bytes.as_ptr() as usize, caller);
        let offset = (block - 7) as usize * 512;
        assert_eq!(bytes, &output[offset..offset + bytes.len()]);
        writes.push((block, bytes.as_ptr() as usize, bytes.len()));
        Ok(())
    })
    .unwrap();
    assert_eq!(provider.allocations.load(Ordering::Relaxed), 2);
    assert_eq!(provider.frees.load(Ordering::Relaxed), 2);
    assert_eq!(writes.len(), 4);
    assert!(
        writes
            .iter()
            .all(|r| r.1 == writes[0].1 && r.2 <= BOUNCE_BYTES)
    );
    assert_eq!(
        reads.iter().map(|r| (r.0, r.2)).collect::<Vec<_>>(),
        writes.iter().map(|r| (r.0, r.2)).collect::<Vec<_>>()
    );
}

#[def_test]
fn word_io_allocation_failure_performs_no_read_or_write() {
    let provider = Provider::with_failure(true);
    let mut storage = [0xa5; 520];
    let output = unaligned(&mut storage, 512);
    assert_eq!(
        read_blocks_with_provider(&provider, 0, 512, 1, output, |_, _| panic!(
            "allocation failure must not reach read"
        )),
        Err(DriverError::NoMemory)
    );
    assert!(output.iter().all(|&byte| byte == 0xa5));
    assert_eq!(
        write_blocks_with_provider(&provider, 0, 512, 1, output, |_, _| panic!(
            "allocation failure must not reach write"
        )),
        Err(DriverError::NoMemory)
    );
    assert_eq!(provider.allocations.load(Ordering::Relaxed), 2);
    assert_eq!(provider.frees.load(Ordering::Relaxed), 0);
}

#[def_test]
fn word_io_transfer_errors_stop_without_retry_and_release_the_bounce() {
    let len = 2 * BOUNCE_BYTES + 512;
    let mut storage = vec![0xa5; len + 8];
    let output = unaligned(&mut storage, len);
    let provider = Provider::new();
    let mut reads = Vec::new();
    let error = read_blocks_with_provider(&provider, 0, 512, 512, output, |block, bytes| {
        reads.push(block);
        bytes.fill(if block == 0 { 0x42 } else { 0xee });
        if block == 0 {
            Ok(())
        } else {
            Err(DriverError::Io)
        }
    });
    assert_eq!(error, Err(DriverError::Io));
    assert_eq!(reads, [0, (BOUNCE_BYTES / 512) as u64]);
    assert_eq!(&output[..BOUNCE_BYTES], vec![0x42; BOUNCE_BYTES]);
    assert!(output[BOUNCE_BYTES..].iter().all(|&byte| byte == 0xa5));
    assert_eq!(provider.frees.load(Ordering::Relaxed), 1);
    let mut writes = Vec::new();
    let error = write_blocks_with_provider(&provider, 0, 512, 512, output, |block, _| {
        writes.push(block);
        if block == 0 {
            Ok(())
        } else {
            Err(DriverError::WouldBlock)
        }
    });
    assert_eq!(error, Err(DriverError::WouldBlock));
    assert_eq!(writes, reads);
    assert_eq!(provider.frees.load(Ordering::Relaxed), 2);

    let mut aligned = staging(test_provider(), 512).unwrap();
    let fail = Provider::with_failure(true);
    assert_eq!(
        read_blocks_with_provider(&fail, 0, 512, 1, aligned.as_mut_slice(), |_, bytes| {
            bytes[..17].fill(0xee);
            Err(DriverError::Io)
        }),
        Err(DriverError::Io)
    );
    assert_eq!(&aligned.as_mut_slice()[..17], &[0xee; 17]);
    assert_eq!(fail.allocations.load(Ordering::Relaxed), 0);
}

#[def_test]
fn word_io_validates_complete_ranges_and_empty_requests_before_transfer() {
    let provider = Provider::with_failure(true);
    let mut storage = [0; 520];
    for (block, block_size, capacity, len) in [
        (0, 0, 8, 512),
        (0, 3, 8, 512),
        (0, 512, 8, 510),
        (8, 512, 8, 512),
        (u64::MAX, 512, u64::MAX, 512),
        (2, 512, 1, 0),
    ] {
        let output = unaligned(&mut storage, len);
        assert_eq!(
            read_blocks_with_provider(
                &provider,
                block,
                block_size,
                capacity,
                output,
                |_, _| panic!("invalid request reached read")
            ),
            Err(DriverError::InvalidInput)
        );
        assert_eq!(
            write_blocks_with_provider(
                &provider,
                block,
                block_size,
                capacity,
                output,
                |_, _| panic!("invalid request reached write")
            ),
            Err(DriverError::InvalidInput)
        );
    }
    read_blocks_with_provider(&provider, 8, 512, 8, &mut [], |_, _| {
        panic!("empty read reached device")
    })
    .unwrap();
    write_blocks_with_provider(&provider, 8, 512, 8, &[], |_, _| {
        panic!("empty write reached device")
    })
    .unwrap();
    assert_eq!(provider.allocations.load(Ordering::Relaxed), 0);
}

#[def_test]
fn word_io_large_device_blocks_are_never_split() {
    let block_size = BOUNCE_BYTES * 2;
    let mut storage = vec![0; 2 * block_size + 8];
    let output = unaligned(&mut storage, 2 * block_size);
    let provider = Provider::new();
    let mut blocks = Vec::new();
    read_blocks_with_provider(&provider, 0, block_size, 2, output, |block, bytes| {
        assert_eq!(bytes.len(), block_size);
        blocks.push(block);
        Ok(())
    })
    .unwrap();
    assert_eq!(blocks, [0, 1]);
    assert_eq!(provider.last_size.load(Ordering::Relaxed), block_size);
    assert_eq!(provider.allocations.load(Ordering::Relaxed), 1);
    assert_eq!(provider.frees.load(Ordering::Relaxed), 1);
}

struct RestrictedDevice(Mutex<Vec<u8>>);

impl BlockDeviceOperations for RestrictedDevice {
    fn block_size(&self) -> usize {
        512
    }

    fn num_blocks(&self) -> u64 {
        8
    }

    fn read_block(&self, block: u64, bytes: &mut [u8]) -> DriverResult {
        read_blocks_with_provider(test_provider(), block, 512, 8, bytes, |block, bytes| {
            assert_eq!(bytes.as_ptr() as usize % 4, 0);
            let start = block as usize * 512;
            bytes.copy_from_slice(&self.0.lock()[start..start + bytes.len()]);
            Ok(())
        })
    }

    fn write_block(&self, block: u64, bytes: &[u8]) -> DriverResult {
        write_blocks_with_provider(test_provider(), block, 512, 8, bytes, |block, bytes| {
            assert_eq!(bytes.as_ptr() as usize % 4, 0);
            let start = block as usize * 512;
            self.0.lock()[start..start + bytes.len()].copy_from_slice(bytes);
            Ok(())
        })
    }

    fn flush(&self) -> DriverResult {
        Ok(())
    }
}

#[def_test]
fn word_io_direct_trait_arc_and_canonical_forwarding_accept_arbitrary_addresses() {
    let backend = Arc::new(RestrictedDevice(Mutex::new(vec![0; 4096])));
    let disk = Gendisk::alloc(
        String::from("word-contract"),
        249,
        0,
        1,
        Box::new(backend.clone()),
    )
    .unwrap();
    let canonical = disk.part0();
    let mut input = [0x73; 520];
    let input = unaligned(&mut input, 512);
    let mut output = [0; 520];
    let output = unaligned(&mut output, 512);
    let direct: &dyn BlockDeviceOperations = backend.as_ref();
    for device in [
        direct,
        &backend as &dyn BlockDeviceOperations,
        canonical.as_ref() as &dyn BlockDeviceOperations,
    ] {
        device.write_block(2, input).unwrap();
        device.read_block(2, output).unwrap();
        assert_eq!(output, input);
        assert_eq!(device.read_block(8, output), Err(DriverError::InvalidInput));
    }
}

#[def_test]
fn word_io_public_provider_failure_precedes_io_and_aligned_requests_bypass_it() {
    let fail = Provider::with_failure(true);
    let mut storage = [0xa5; 520];
    let output = unaligned(&mut storage, 512);
    assert_eq!(
        read_blocks_with_provider(&fail, 0, 512, 1, output, |_, _| panic!(
            "provider failure reached read"
        )),
        Err(DriverError::NoMemory)
    );
    assert!(output.iter().all(|&byte| byte == 0xa5));
    assert_eq!(
        write_blocks_with_provider(&fail, 0, 512, 1, output, |_, _| panic!(
            "provider failure reached write"
        )),
        Err(DriverError::NoMemory)
    );
    assert_eq!(fail.allocations.load(Ordering::Relaxed), 2);

    let mut owner = staging(test_provider(), 512).unwrap();
    let output = owner.as_mut_slice();
    let address = output.as_ptr() as usize;
    read_blocks_with_provider(&fail, 0, 512, 1, output, |_, bytes| {
        assert_eq!(bytes.as_ptr() as usize, address);
        bytes.fill(0x73);
        Ok(())
    })
    .unwrap();
    assert_eq!(
        write_blocks_with_provider(&fail, 0, 512, 1, output, |_, bytes| {
            assert_eq!(bytes.as_ptr() as usize, address);
            assert_eq!(bytes, &[0x73; 512]);
            Err(DriverError::Io)
        }),
        Err(DriverError::Io)
    );
    assert_eq!(fail.allocations.load(Ordering::Relaxed), 2);
    assert_eq!(fail.frees.load(Ordering::Relaxed), 0);
}

#[def_test]
fn word_io_public_provider_releases_owned_memory_on_success_and_io_errors() {
    let owner = Provider::new();
    let mut storage = [0xa5; 520];
    let output = unaligned(&mut storage, 512);
    let caller = output.as_ptr() as usize;
    read_blocks_with_provider(&owner, 0, 512, 1, output, |_, bytes| {
        assert_eq!(bytes.as_ptr() as usize % align_of::<u32>(), 0);
        assert_ne!(bytes.as_ptr() as usize, caller);
        bytes.fill(0x73);
        Ok(())
    })
    .unwrap();
    assert_eq!(output, &[0x73; 512]);
    assert_eq!(owner.allocations.load(Ordering::Relaxed), 1);
    assert_eq!(owner.frees.load(Ordering::Relaxed), 1);

    assert_eq!(
        read_blocks_with_provider(&owner, 0, 512, 1, output, |_, bytes| {
            bytes.fill(0xee);
            Err(DriverError::Io)
        }),
        Err(DriverError::Io)
    );
    assert_eq!(output, &[0x73; 512]);
    assert_eq!(owner.allocations.load(Ordering::Relaxed), 2);
    assert_eq!(owner.frees.load(Ordering::Relaxed), 2);
    assert_eq!(
        write_blocks_with_provider(&owner, 0, 512, 1, output, |_, bytes| {
            assert_eq!(bytes, &[0x73; 512]);
            Err(DriverError::WouldBlock)
        }),
        Err(DriverError::WouldBlock)
    );
    assert_eq!(owner.allocations.load(Ordering::Relaxed), 3);
    assert_eq!(owner.frees.load(Ordering::Relaxed), 3);
}
