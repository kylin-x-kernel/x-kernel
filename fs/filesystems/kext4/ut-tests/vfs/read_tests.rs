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
    FilesystemBlock,
    file::read_tests::{ReadDevice, read_fixture},
    io::FilesystemDevice,
    superblock::test_fixtures::{
        TEST_FREE_BLOCKS, allocate_checkpointed_regular_inode, install_test_internal_journal,
        journal_allocator_test_filesystem, journal_allocator_test_filesystem_with_block_size,
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
    let block_size = u64::from(filesystem.layout().block_size());
    let super_block = SuperBlock::new_with_flags_and_private(
        &crate::FILE_SYSTEM_TYPE,
        &EXT4_SUPER_OPERATIONS,
        kalloc_types::KBox::new(Ext4Mount::new(filesystem)).unwrap(),
        flags,
        block_size,
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
    mapping
        .with_folio(0, |folio| assert!(folio.is_none()))
        .unwrap();
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
fn ext4_failed_readahead_keeps_failed_pages_stale_and_buffered_retry_is_complete() {
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
        mapping
            .with_folio(index as u64, |folio| {
                if (3..6).contains(&index) {
                    assert!(folio.unwrap().data().iter().all(|&byte| byte == 0));
                } else {
                    assert!(
                        folio.is_none(),
                        "failed I/O must not publish its modified prefix"
                    );
                }
            })
            .unwrap();
    }
    // Consumed tokens remain resident for retry. Holes and unwritten extents
    // are independently complete even when the data bios fail.
    assert_eq!(
        mapping.nrpages(),
        expected.len().div_ceil(PAGE_SIZE_4K) as u64
    );
    *device.fail_reads.lock() = false;
    assert_eq!(
        file.read_from(&mut output, &mut position),
        Ok(expected.len())
    );
    assert_eq!(&output[..expected.len()], expected.as_slice());
    assert!(output[expected.len()..].iter().all(|&byte| byte == 0xa5));
    assert_eq!(position, expected.len() as u64);
    let last_index = (expected.len() / PAGE_SIZE_4K) as u64;
    mapping
        .with_folio(last_index, |folio| {
            let folio = folio.unwrap();
            assert!(folio.is_uptodate());
            assert!(!folio.is_dirty());
            assert_eq!(&folio.data()[..17], &[0x7a; 17]);
            assert!(folio.data()[17..].iter().all(|&byte| byte == 0));
        })
        .unwrap();
}

#[def_test]
fn ext4_readahead_submits_original_pages_and_demand_waits_only_for_its_bio() {
    let (super_block, file, device, expected) = cached_read_fixture();
    *device.pending_reads.lock() = Some(Vec::new());
    let reader_file = file.clone();
    let reader = ktask::spawn(move || {
        let mut output = vec![0; 3 * PAGE_SIZE_4K];
        assert_eq!(reader_file.read_from(&mut output, &mut 0), Ok(output.len()));
        assert_eq!(output, expected[..output.len()]);
    });
    // The window contains two physical runs, separated by holes. Both must
    // reach the device before either completes; no clock-based rendezvous.
    device
        .queued
        .wait_until(|| device.pending_reads.lock().as_ref().unwrap().len() == 2);
    let mut pending = device.pending_reads.lock().take().unwrap();
    let tail = pending.pop().unwrap();
    let mut first = pending.pop().unwrap();
    let addresses: Vec<_> = first
        .vectors_mut()
        .map(|vector| vector.bytes().as_ptr() as usize)
        .collect();
    assert_eq!(addresses.len(), 3);
    device.complete_bio(first);
    assert_eq!(
        reader.join(),
        0,
        "demand must not wait for the speculative tail"
    );
    for (index, address) in addresses.into_iter().enumerate() {
        file.mapping()
            .with_folio(index as u64, |folio| {
                assert_eq!(folio.unwrap().data().as_ptr() as usize, address);
            })
            .unwrap();
    }
    // A later completion cannot acquire the filesystem lock held here. It
    // must finish before explicit cache teardown can retire the tail page.
    {
        let _filesystem = ext4_private(&super_block).unwrap().write(&super_block);
        device.complete_bio(tail);
    }
    file.mapping().truncate_final().unwrap();
    assert_eq!(file.mapping().nrpages(), 0);
}

#[def_test]
fn ext4_subpage_hole_and_unwritten_layout_reads_complete_folios() {
    let (filesystem, private, _device, expected) = read_fixture(1024, 512);
    let (_super_block, file) = cached_file(
        filesystem,
        private,
        SuperBlockFlags::RDONLY,
        OpenFlags::empty(),
    );
    // Page 0 ends in a hole; page 1 has unwritten blocks before mapped data.
    let mut output = vec![0xa5; expected.len() + 19];
    assert_eq!(file.read_from(&mut output, &mut 0), Ok(expected.len()));
    assert_eq!(&output[..expected.len()], expected.as_slice());
    assert!(output[expected.len()..].iter().all(|&byte| byte == 0xa5));
    file.mapping()
        .with_folio(1, |folio| {
            assert!(
                folio.unwrap().data()[expected.len() % PAGE_SIZE_4K..]
                    .iter()
                    .all(|&byte| byte == 0)
            );
        })
        .unwrap();
}

#[def_test]
fn ext4_fragmented_folio_submits_all_parts_before_completion_and_retries_failure() {
    for (block_size, outcome) in [1024, 2048]
        .into_iter()
        .flat_map(|size| (0..3).map(move |outcome| (size, outcome)))
    {
        let second_logical = (2048 / block_size) as u32;
        let (filesystem, private, device, expected) =
            crate::file::read_tests::read_fixture_with_extents(
                block_size,
                512,
                32,
                &[(0, 1, 20), (second_logical, 1, 25), (4, 0x8002, 23)],
            );
        let (super_block, file) = cached_file(
            filesystem,
            private,
            SuperBlockFlags::RDONLY,
            OpenFlags::empty(),
        );
        *device.pending_reads.lock() = Some(Vec::new());
        let reader_file = file.clone();
        let expected_page = expected[..PAGE_SIZE_4K].to_vec();
        let reader = ktask::spawn(move || {
            let result = reader_file.mapping().with_folio_or_create(0, |folio| {
                assert_eq!(folio.data(), expected_page.as_slice());
                Ok(())
            });
            assert_eq!(
                result,
                if outcome == 0 {
                    Ok(())
                } else {
                    Err(VfsError::Io)
                }
            );
        });
        device
            .queued
            .wait_until(|| device.pending_reads.lock().as_ref().unwrap().len() == 2);
        let mut pending = device.pending_reads.lock().take().unwrap();
        let mut last = pending.pop().unwrap();
        let mut first = pending.pop().unwrap();
        let first_address = first.vectors_mut().next().unwrap().bytes().as_ptr() as usize;
        let last_address = last.vectors_mut().next().unwrap().bytes().as_ptr() as usize;
        assert_eq!(last_address, first_address + 2048);
        {
            let _filesystem = ext4_private(&super_block).unwrap().write(&super_block);
            device.complete_bio(last);
            match outcome {
                0 => device.complete_bio(first),
                1 => {
                    *device.fail_reads.lock() = true;
                    device.complete_bio(first);
                }
                _ => drop(first),
            }
        }
        assert_eq!(reader.join(), 0);
        *device.fail_reads.lock() = false;
        file.mapping()
            .with_folio_or_create(0, |folio| {
                assert_eq!(folio.data(), &expected[..PAGE_SIZE_4K]);
                assert_eq!(folio.data().as_ptr() as usize, first_address);
                Ok(())
            })
            .unwrap();
    }
}

#[def_test]
fn ext4_fragmented_reads_submit_maximal_contiguous_runs() {
    for (index, extents, expected_runs) in [
        (0, vec![(1, 3, 20)], vec![(20, 1024, 3072)]),
        (0, vec![(0, 0x8001, 23), (1, 3, 20)], vec![(20, 1024, 3072)]),
        (
            0,
            vec![(0, 2, 20), (2, 2, 25)],
            vec![(20, 0, 2048), (25, 2048, 2048)],
        ),
        (
            0,
            vec![(0, 1, 20), (2, 2, 21)],
            vec![(20, 0, 1024), (21, 2048, 2048)],
        ),
        (0, vec![(1, 2, 20)], vec![(20, 1024, 2048)]),
        // EOF falls inside the second data block after a leading hole.
        (1, vec![(5, 2, 20)], vec![(20, 1024, 2048)]),
    ] {
        let (filesystem, private, device, expected) =
            crate::file::read_tests::read_fixture_with_extents(1024, 512, 32, &extents);
        let (_super_block, file) = cached_file(
            filesystem,
            private,
            SuperBlockFlags::RDONLY,
            OpenFlags::empty(),
        );
        *device.pending_reads.lock() = Some(Vec::new());
        let reader_file = file.clone();
        let reader = ktask::spawn(move || {
            reader_file
                .mapping()
                .with_folio_or_create(index, |_| Ok(()))
                .unwrap();
        });
        device.queued.wait_until(|| {
            device.pending_reads.lock().as_ref().unwrap().len() == expected_runs.len()
        });
        let pending = device.pending_reads.lock().take().unwrap();
        let mut page_address = None;
        for (mut bio, (physical, offset_bytes, len_bytes)) in
            pending.into_iter().zip(expected_runs).rev()
        {
            assert_eq!(bio.sector(), physical * 2);
            {
                let mut vectors = bio.vectors_mut();
                let vector = vectors.next().unwrap();
                let bytes = vector.bytes();
                assert_eq!(bytes.len(), len_bytes);
                let address = bytes.as_ptr() as usize - offset_bytes;
                assert_eq!(*page_address.get_or_insert(address), address);
                assert!(vectors.next().is_none());
            }
            device.complete_bio(bio);
        }
        assert_eq!(reader.join(), 0);
        file.mapping()
            .with_folio(index, |folio| {
                let folio = folio.unwrap();
                assert_eq!(folio.data().as_ptr() as usize, page_address.unwrap());
                let start = index as usize * PAGE_SIZE_4K;
                let valid_bytes = (expected.len() - start).min(PAGE_SIZE_4K);
                assert_eq!(
                    &folio.data()[..valid_bytes],
                    &expected[start..start + valid_bytes]
                );
                assert!(folio.data()[valid_bytes..].iter().all(|&byte| byte == 0));
            })
            .unwrap();
    }
}

fn fragmented_truncate_fixture(
    block_size: usize,
) -> (Arc<SuperBlock>, Arc<VfsFile>, Arc<ReadDevice>, Vec<u8>) {
    use crate::{
        BlockCount, BlockGroupNumber, PhysicalBlock,
        extent::ExtentMappingState,
        file::RegularWriteMetadata,
        jbd2::{JournalCredits, JournalTransactions, TransactionId},
    };

    let (mut filesystem, storage) =
        journal_allocator_test_filesystem_with_block_size(block_size, TEST_FREE_BLOCKS, 0xff);
    let private = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 8082);
    let blocks_per_page = PAGE_SIZE_4K / block_size;
    let last_logical = blocks_per_page + 2048 / block_size;
    let journal = JournalTransactions::new(TransactionId::new(8083));
    let mut handle = journal.begin(JournalCredits::new(32)).unwrap();
    let mut expected = vec![0; 2 * PAGE_SIZE_4K];
    for (logical, len_blocks, physical, byte) in [
        (0, blocks_per_page + 1, 1040, 0x5a),
        (last_logical, 1, 1040 + blocks_per_page + 2, 0x7b),
    ] {
        let physical = FilesystemBlock::new(physical as u64);
        let allocation = filesystem
            .allocate_block_run_in_group(
                BlockGroupNumber::new(0),
                Some(physical),
                BlockCount::new(len_blocks as u32),
                BlockCount::new(len_blocks as u32),
                &mut handle,
            )
            .unwrap();
        assert_eq!(allocation.first_block().get(), physical.get());
        private.add_allocated_bytes((len_blocks * block_size) as u64);
        filesystem
            .insert_extent_mapping(
                &private,
                LogicalBlock::new(logical as u64),
                PhysicalBlock::new(physical.get()),
                BlockCount::new(len_blocks as u32),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
        let data = vec![byte; len_blocks * block_size];
        filesystem
            .device
            .write_contiguous_blocks(physical, len_blocks as u32, &data)
            .unwrap();
        expected[logical * block_size..(logical + len_blocks) * block_size].copy_from_slice(&data);
    }
    filesystem
        .update_regular_inode_write_metadata(
            &private,
            expected.len() as u64,
            RegularWriteMetadata::SizeOnly,
            &mut handle,
        )
        .unwrap();
    drop(handle);
    let commit = journal.force_commit(TransactionId::new(8083)).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
    private.set_size(expected.len() as u64);
    let device = ReadDevice::new(crate::io::test_block_device(storage));
    filesystem.device = Arc::new(
        FilesystemDevice::open(
            crate::io::test_block_device(device.clone()),
            block_size,
            device.num_blocks(),
        )
        .unwrap(),
    );
    let (super_block, file) = cached_file(
        filesystem,
        private,
        SuperBlockFlags::empty(),
        OpenFlags::READ_WRITE,
    );
    (super_block, file, device, expected)
}

#[def_test]
fn ext4_truncate_drains_fragmented_readahead_before_freeing_blocks() {
    for (block_size, fails_first) in [1024, 2048]
        .into_iter()
        .flat_map(|size| [false, true].map(move |fails| (size, fails)))
    {
        let (super_block, file, device, expected) = fragmented_truncate_fixture(block_size);
        let tail_logicals = [
            PAGE_SIZE_4K / block_size,
            PAGE_SIZE_4K / block_size + 2048 / block_size,
        ];
        let (free_before, tail_blocks) = {
            let state = ext4_private(&super_block).unwrap().read();
            let private = file.inode().private::<Ext4Inode>().unwrap();
            let blocks = tail_logicals.map(|logical| {
                match state
                    .map_blocks(private, LogicalBlock::new(logical as u64))
                    .unwrap()
                {
                    BlockMapping::Mapped { physical, .. } => physical,
                    _ => panic!("fixture must map both tail runs"),
                }
            });
            (state.free_blocks_count(), blocks)
        };
        *device.pending_reads.lock() = Some(Vec::new());
        let reader_file = file.clone();
        let expected_first = expected[..PAGE_SIZE_4K].to_vec();
        let reader = ktask::spawn(move || {
            let mut output = vec![0; PAGE_SIZE_4K];
            assert_eq!(reader_file.read_from(&mut output, &mut 0), Ok(output.len()));
            assert_eq!(output, expected_first);
        });
        device
            .queued
            .wait_until(|| device.pending_reads.lock().as_ref().unwrap().len() == 3);
        let mut pending = device.pending_reads.lock().take().unwrap();
        let last = pending.pop().unwrap();
        let first_tail = pending.pop().unwrap();
        device.complete_bio(pending.pop().unwrap());
        assert_eq!(
            reader.join(),
            0,
            "demand must return while both tail runs remain in flight"
        );

        let new_len = (PAGE_SIZE_4K + 17) as u64;
        let inode = file.inode().clone();
        let truncator_file = file.clone();
        let truncator = ktask::spawn(move || truncator_file.truncate(new_len).unwrap());
        // Once i_size is published, this task has no other asynchronous work:
        // a blocked state means cache truncation is waiting for the tail folio.
        // Its completion is controlled solely by the two bios retained here.
        while inode.size() != new_len || !truncator.state().is_blocked() {
            assert_ne!(
                truncator.state(),
                ktask::TaskState::Exited,
                "truncate skipped the in-flight folio"
            );
            ktask::yield_now();
        }
        let assert_blocks_retained = || {
            let state = ext4_private(&super_block).unwrap().read();
            assert_eq!(state.free_blocks_count(), free_before);
            let private = inode.private::<Ext4Inode>().unwrap();
            for (logical, physical) in tail_logicals.into_iter().zip(&tail_blocks) {
                assert!(
                    matches!(state.map_blocks(private, LogicalBlock::new(logical as u64)).unwrap(),
                    BlockMapping::Mapped { physical: actual, .. } if actual == *physical)
                );
            }
        };
        assert_blocks_retained();
        *device.fail_reads.lock() = fails_first;
        device.complete_bio(first_tail);
        *device.fail_reads.lock() = false;
        assert_blocks_retained();
        assert!(truncator.state().is_blocked());
        device.complete_bio(last);
        assert_eq!(truncator.join(), 0);
        {
            let state = ext4_private(&super_block).unwrap().read();
            assert_eq!(state.free_blocks_count(), free_before + 1);
            assert!(matches!(
                state
                    .map_blocks(
                        inode.private::<Ext4Inode>().unwrap(),
                        LogicalBlock::new((PAGE_SIZE_4K / block_size + 2048 / block_size) as u64)
                    )
                    .unwrap(),
                BlockMapping::Hole { .. }
            ));
        }
        file.truncate(expected.len() as u64).unwrap();
        file.mapping()
            .with_folio_or_create(1, |folio| {
                assert_eq!(
                    &folio.data()[..17],
                    &expected[PAGE_SIZE_4K..PAGE_SIZE_4K + 17]
                );
                assert!(folio.data()[17..].iter().all(|&byte| byte == 0));
                Ok(())
            })
            .unwrap();
    }
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
    mapping
        .with_folio(0, |folio| assert!(folio.is_none()))
        .unwrap();

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
    mapping
        .with_folio(0, |folio| {
            let folio = folio.unwrap();
            assert!(folio.is_uptodate());
            assert!(folio.is_dirty());
            assert_eq!(folio.data(), expected.as_slice());
        })
        .unwrap();
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

#[derive(Debug, Default)]
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
