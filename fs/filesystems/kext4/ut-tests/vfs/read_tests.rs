// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Real ext4/PageCache error publication and retry integration tests.

use alloc::{string::String, sync::Arc, vec, vec::Vec};

use block::BlockDeviceOperations;
use kvfs::{Mount, OpenFlags, Path};
use unittest::def_test;

use super::*;
use crate::{
    file::read_tests::{ReadDevice, read_fixture},
    io::FilesystemDevice,
    superblock::test_fixtures::{
        TEST_FREE_BLOCKS, allocate_checkpointed_regular_inode, install_test_internal_journal,
        journal_allocator_test_filesystem,
    },
};

#[expect(
    clippy::unwrap_used,
    reason = "CG-021-no-panic-on-reachable-failure: test scenario: setup must bind a real ext4 \
              inode, superblock and open-file readahead policy or abort the fixture"
)]
pub(super) fn cached_file(
    filesystem: Ext4SbInfo,
    private: Ext4Inode,
    flags: SuperBlockFlags,
    open_flags: OpenFlags,
) -> (Arc<SuperBlock>, Arc<VfsFile>) {
    let super_block = SuperBlock::new_with_flags_and_private(
        &crate::FILE_SYSTEM_TYPE,
        &EXT4_SUPER_OPERATIONS,
        Ext4Mount::new(filesystem),
        flags,
        PAGE_SIZE_4K as u64,
        u64::MAX,
        |super_block| {
            let inode = Ext4SuperOperations::iget_from_private(super_block, private).unwrap();
            Dentry::new_file_from_inode(inode, None, String::new())
        },
    );
    let path = Path::new(Mount::new_root(&super_block), super_block.root_dir());
    let file = kvfs::dentry_open(path, open_flags.bits(), kcred::initial_cred()).unwrap();
    (super_block, file)
}

fn cached_read_fixture() -> (Arc<SuperBlock>, Arc<VfsFile>, Arc<ReadDevice>, Vec<u8>) {
    let (filesystem, private, device, expected) = read_fixture(PAGE_SIZE_4K, 512);
    let (super_block, file) = cached_file(
        filesystem,
        private,
        SuperBlockFlags::RDONLY,
        OpenFlags::empty(),
    );
    (super_block, file, device, expected)
}

#[def_test]
fn ext4_failed_read_folio_stays_uninitialized_until_complete_retry() {
    let (_super_block, file, device, expected) = cached_read_fixture();
    let mapping = file.mapping();
    *device.fail_reads.lock() = true;
    assert_eq!(
        mapping.with_folio_or_create(0, |_| panic!("failed read must not expose its folio")),
        Err::<(), _>(VfsError::Io)
    );
    // The failed folio is resident, but with_folio hides non-uptodate data.
    assert_eq!(mapping.nrpages(), 1);
    mapping.with_folio(0, |folio| assert!(folio.is_none()));
    assert_eq!(device.destinations.lock().len(), 1);
    *device.fail_reads.lock() = false;
    assert_eq!(
        mapping.with_folio_or_create(0, |folio| {
            assert!(folio.is_uptodate());
            assert!(!folio.is_dirty());
            assert_eq!(folio.data(), &expected[..PAGE_SIZE_4K]);
            Ok(())
        }),
        Ok(())
    );
    assert_eq!(device.destinations.lock().len(), 2);
}

#[def_test]
fn ext4_failed_readahead_does_not_publish_prefix_and_buffered_retry_is_complete() {
    let (_super_block, file, device, expected) = cached_read_fixture();
    let mapping = file.mapping();
    let mut output = vec![0xa5; expected.len() + 19];
    let mut position = 0;
    *device.fail_reads.lock() = true;
    assert_eq!(
        file.read_from(&mut output, &mut position),
        Err(VfsError::Io)
    );
    assert_eq!(position, 0);
    assert!(output.iter().all(|&byte| byte == 0xa5));
    let reads = core::mem::take(&mut *device.destinations.lock());
    assert!(reads.iter().any(|&(_, _, len)| len >= 2 * PAGE_SIZE_4K));
    for index in 0..expected.len().div_ceil(PAGE_SIZE_4K) {
        mapping.with_folio(index as u64, |folio| assert!(folio.is_none()));
    }
    // Failed batch reservations are retired; demand retry retains one
    // non-uptodate resident. No modified prefix is readable through the cache.
    assert_eq!(mapping.nrpages(), 1);
    *device.fail_reads.lock() = false;
    assert_eq!(
        file.read_from(&mut output, &mut position),
        Ok(expected.len())
    );
    assert_eq!(&output[..expected.len()], expected.as_slice());
    assert!(output[expected.len()..].iter().all(|&byte| byte == 0xa5));
    assert_eq!(position, expected.len() as u64);
    let last_index = (expected.len() / PAGE_SIZE_4K) as u64;
    mapping.with_folio(last_index, |folio| {
        let folio = folio.unwrap();
        assert!(folio.is_uptodate());
        assert!(!folio.is_dirty());
        assert_eq!(&folio.data()[..17], &[0x7a; 17]);
        assert!(folio.data()[17..].iter().all(|&byte| byte == 0));
    });
}

#[def_test]
fn ext4_failed_read_then_partial_write_reinitializes_before_readback() {
    let (mut filesystem, storage) =
        journal_allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let private = Arc::new(allocate_checkpointed_regular_inode(&mut filesystem));
    install_test_internal_journal(&mut filesystem, 8081);
    let mut expected = vec![0x41; PAGE_SIZE_4K];
    private.set_size(PAGE_SIZE_4K as u64);
    filesystem
        .writeback_ordered_at(
            &private,
            0,
            &expected,
            PAGE_SIZE_4K as u64,
            Ext4SyncIntent::DataOnly,
            || true,
        )
        .unwrap();
    let data_block = match filesystem
        .map_blocks(&private, LogicalBlock::new(0))
        .unwrap()
    {
        BlockMapping::Mapped { physical, .. } => physical.get(),
        _ => panic!("fixture must map its data block"),
    };
    let device = ReadDevice::new(crate::io::test_block_device(storage));
    // Fail only the folio read; write-time timestamp updates must still be able
    // to read journal metadata without aborting the filesystem.
    *device.read_failure_block.lock() = Some(data_block);
    filesystem.device = Arc::new(
        FilesystemDevice::open(
            crate::io::test_block_device(device.clone()),
            PAGE_SIZE_4K,
            device.num_blocks(),
        )
        .unwrap(),
    );
    let (_super_block, file) = cached_file(
        filesystem,
        Arc::into_inner(private).expect("test inode Arc is unique"),
        SuperBlockFlags::empty(),
        OpenFlags::READ_WRITE,
    );
    let mapping = file.mapping();
    let mut output = vec![0xa5; PAGE_SIZE_4K];
    *device.fail_reads.lock() = true;
    assert_eq!(file.read_from(&mut output, &mut 0), Err(VfsError::Io));
    assert!(output.iter().all(|&byte| byte == 0xa5));
    let mut position = 64;
    assert_eq!(file.write_from(b"new", &mut position), Err(VfsError::Io));
    assert_eq!(position, 64);
    mapping.with_folio(0, |folio| assert!(folio.is_none()));

    // A partial write must retry the backing read before copying its bytes;
    // otherwise the earlier 0xee prefix could become visible as dirty data.
    *device.fail_reads.lock() = false;
    let data_reads = || {
        device
            .destinations
            .lock()
            .iter()
            .filter(|(block, ..)| *block == data_block)
            .count()
    };
    let reads_before_retry = data_reads();
    assert_eq!(file.write_from(b"new", &mut position), Ok(3));
    assert_eq!(position, 67);
    assert_eq!(data_reads(), reads_before_retry + 1);
    expected[64..67].copy_from_slice(b"new");
    mapping.with_folio(0, |folio| {
        let folio = folio.unwrap();
        assert!(folio.is_uptodate());
        assert!(folio.is_dirty());
        assert_eq!(folio.data(), expected.as_slice());
    });
    assert_eq!(file.read_from(&mut output, &mut 0), Ok(PAGE_SIZE_4K));
    assert_eq!(output, expected);
}

#[def_test]
fn read_only_mount_access_reclaims_metadata_without_checkpoint_activity() {
    // 1152 resident 4-KiB blocks exceed the 4-MiB retention allowance.
    let (filesystem, private, device, _) =
        crate::file::read_tests::read_fixture_with_blocks(PAGE_SIZE_4K, 512, 1152);
    let (super_block, _file) = cached_file(
        filesystem,
        private,
        SuperBlockFlags::RDONLY,
        OpenFlags::empty(),
    );
    let mount = ext4_private(&super_block).unwrap();
    {
        let state = mount.read();
        state.metadata_io.invalidate_all();
        for block in 0..1152 {
            drop(
                state
                    .read_metadata_block(crate::FilesystemBlock::new(block))
                    .unwrap(),
            );
        }
        // Reading metadata itself never evicts; maintenance starts at drop.
        assert_eq!(state.metadata_io.cached_block_count(), 1152);
    }
    let reads = device.destinations.lock().len();
    {
        let state = mount.read();
        assert_eq!(state.metadata_io.cached_block_count(), 1088);
    }
    {
        let state = mount.read();
        assert_eq!(state.metadata_io.cached_block_count(), 1024);
    }
    assert_eq!(device.destinations.lock().len(), reads);
}

#[def_test]
fn mount_cache_maintenance_preserves_allocator_prefetch_window() {
    let (filesystem, private, device, _) =
        crate::file::read_tests::read_fixture_with_blocks(PAGE_SIZE_4K, 512, 1152);
    let (super_block, _file) = cached_file(
        filesystem,
        private,
        SuperBlockFlags::RDONLY,
        OpenFlags::empty(),
    );
    let mount = ext4_private(&super_block).unwrap();
    {
        let state = mount.write(&super_block);
        state.metadata_io.invalidate_all();
        let block = crate::FilesystemBlock::new(0);
        state.prefetch_metadata_blocks(&[block]).unwrap();
        for index in 1..1152 {
            drop(
                state
                    .read_metadata_block(crate::FilesystemBlock::new(index))
                    .unwrap(),
            );
        }
        let reads = device.destinations.lock().len();
        // Pressure cannot evict the prefetched slot before the allocator's
        // authoritative re-read inside the same mounted operation.
        let _allocator = crate::sync::mutex_lock(&state.allocator);
        drop(state.read_metadata_block(block).unwrap());
        assert_eq!(device.destinations.lock().len(), reads);
        assert_eq!(state.metadata_io.cached_block_count(), 1152);
    }
    let state = mount.read();
    // The authoritative re-read earns block 0 a second chance; the other 63
    // inspected entries can retire after the allocator window closes.
    assert_eq!(state.metadata_io.cached_block_count(), 1089);
}

#[derive(Clone, Copy)]
enum MetadataReadPattern {
    Cyclic,
    HotAndCold { first_hot: usize },
}

enum MetadataReclaimBoundary {
    EachAccess,
    EndOfTrace,
}

#[derive(Default, Debug)]
struct MetadataReadCounts {
    hot_reloads: usize,
    total_reloads: usize,
    resident_blocks: usize,
}

/// Counts I/O after warming the same metadata trace. Holding the reference guard
/// until the end models the old read-only path without periodic cache eviction;
/// the other run drops a real mount guard after every logical access.
fn metadata_read_trace(
    pattern: MetadataReadPattern,
    boundary: MetadataReclaimBoundary,
) -> MetadataReadCounts {
    const WARM_BLOCKS: usize = 1152;
    const BATCH_BLOCKS: usize = 32;
    let (filesystem, _inode, device, _) =
        crate::file::read_tests::read_fixture_with_blocks(PAGE_SIZE_4K, 512, 2304);
    let mount = Ext4Mount::new(filesystem);
    let reference = match boundary {
        MetadataReclaimBoundary::EachAccess => None,
        MetadataReclaimBoundary::EndOfTrace => Some(mount.read()),
    };
    let read_batch = |hot: core::ops::Range<usize>, cold: core::ops::Range<usize>| {
        let access;
        let state = match reference.as_ref() {
            Some(state) => state,
            None => {
                access = mount.read();
                &access
            }
        };
        let before = device.destinations.lock().len();
        for block in hot {
            let bytes = state
                .read_metadata_block(crate::FilesystemBlock::new(block as u64))
                .unwrap();
            assert_eq!(bytes.as_ref().len(), PAGE_SIZE_4K);
        }
        let hot_reloads = device.destinations.lock().len() - before;
        for block in cold {
            drop(
                state
                    .read_metadata_block(crate::FilesystemBlock::new(block as u64))
                    .unwrap(),
            );
        }
        MetadataReadCounts {
            hot_reloads,
            total_reloads: device.destinations.lock().len() - before,
            resident_blocks: state.metadata_io.cached_block_count(),
        }
    };
    read_batch(0..0, 0..WARM_BLOCKS);
    // Warm the hot subset again after the first maintenance passes. Subsequent
    // hot misses then measure eviction during the trace, not initial warmup.
    let first_hot = match pattern {
        MetadataReadPattern::Cyclic => 0,
        MetadataReadPattern::HotAndCold { first_hot } => first_hot,
    };
    read_batch(first_hot..first_hot + BATCH_BLOCKS, 0..0);
    let mut counts = MetadataReadCounts::default();
    match pattern {
        MetadataReadPattern::Cyclic => {
            for _ in 0..4 {
                for first in (0..WARM_BLOCKS).step_by(BATCH_BLOCKS) {
                    let batch = read_batch(first..first + BATCH_BLOCKS, 0..0);
                    counts.hot_reloads += batch.hot_reloads;
                    counts.total_reloads += batch.total_reloads;
                }
            }
        }
        MetadataReadPattern::HotAndCold { first_hot } => {
            for round in 0..32 {
                let first = WARM_BLOCKS + round * BATCH_BLOCKS;
                let batch = read_batch(
                    first_hot..first_hot + BATCH_BLOCKS,
                    first..first + BATCH_BLOCKS,
                );
                counts.hot_reloads += batch.hot_reloads;
                counts.total_reloads += batch.total_reloads;
            }
        }
    }
    counts.resident_blocks = match reference.as_ref() {
        Some(state) => state.metadata_io.cached_block_count(),
        None => mount.read().metadata_io.cached_block_count(),
    };
    counts
}

#[def_test]
fn metadata_cache_cyclic_reads_measure_capacity_misses_after_warmup() {
    let reference = metadata_read_trace(
        MetadataReadPattern::Cyclic,
        MetadataReclaimBoundary::EndOfTrace,
    );
    let mounted = metadata_read_trace(
        MetadataReadPattern::Cyclic,
        MetadataReclaimBoundary::EachAccess,
    );
    klogger::warn!("metadata cyclic reads: reference={reference:?}, mounted={mounted:?}");
    assert_eq!(reference.total_reloads, 0);
    assert!(mounted.total_reloads > 0);
}

#[def_test]
fn metadata_cache_retains_reused_hot_blocks_during_cold_stream() {
    // Heat must not depend on whether a physical key happens to precede or
    // follow the reclaim cursor when the workload starts.
    for first_hot in [0, 128, 768] {
        let pattern = MetadataReadPattern::HotAndCold { first_hot };
        let reference = metadata_read_trace(pattern, MetadataReclaimBoundary::EndOfTrace);
        let mounted = metadata_read_trace(pattern, MetadataReclaimBoundary::EachAccess);
        klogger::warn!(
            "metadata hot/cold reads at {first_hot}: reference={reference:?}, mounted={mounted:?}"
        );
        assert_eq!(reference.hot_reloads, 0);
        assert_eq!(reference.total_reloads, 1024);
        assert_eq!(mounted.hot_reloads, 0);
        assert_eq!(mounted.total_reloads, reference.total_reloads);
        assert!(mounted.resident_blocks <= 1024);
    }
}

#[def_test]
fn allocator_prefetch_pins_survive_reclaim_without_mount_exclusion() {
    let (filesystem, _private, device, _) =
        crate::file::read_tests::read_fixture_with_blocks(PAGE_SIZE_4K, 512, 1152);
    let block = crate::FilesystemBlock::new(0);
    let pins = filesystem.prefetch_metadata_blocks(&[block]).unwrap();
    for index in 1..1152 {
        drop(
            filesystem
                .read_metadata_block(crate::FilesystemBlock::new(index))
                .unwrap(),
        );
    }
    let reads = device.destinations.lock().len();
    filesystem.metadata_io.reclaim_unused(usize::MAX);
    {
        let _allocator = crate::sync::mutex_lock(&filesystem.allocator);
        drop(filesystem.read_metadata_block(block).unwrap());
    }
    assert_eq!(device.destinations.lock().len(), reads);
    drop(pins);
    filesystem.metadata_io.reclaim_unused(usize::MAX);
    drop(filesystem.read_metadata_block(block).unwrap());
    assert_eq!(device.destinations.lock().len(), reads + 1);
}
