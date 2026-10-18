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
fn cached_file(
    filesystem: Ext4SbInfo,
    private: Ext4Inode,
    flags: SuperBlockFlags,
    open_flags: OpenFlags,
) -> (Arc<SuperBlock>, Arc<VfsFile>) {
    let super_block = SuperBlock::new_with_flags_and_private(
        &crate::FILE_SYSTEM_TYPE,
        &EXT4_SUPER_OPERATIONS,
        RwLock::new(filesystem),
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
    let private = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 8081);
    let mut expected = vec![0x41; PAGE_SIZE_4K];
    private.set_size(PAGE_SIZE_4K as u64);
    filesystem
        .writeback_ordered_at(
            &private,
            0,
            &expected,
            PAGE_SIZE_4K as u64,
            Ext4Timestamp::new(1234, 0),
            Ext4SyncIntent::DataOnly,
        )
        .unwrap();
    let device = ReadDevice::new(storage);
    filesystem.device = Arc::new(
        FilesystemDevice::open(device.clone(), PAGE_SIZE_4K, device.num_blocks()).unwrap(),
    );
    let (_super_block, file) = cached_file(
        filesystem,
        private,
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
    let reads_before_retry = device.destinations.lock().len();
    assert_eq!(file.write_from(b"new", &mut position), Ok(3));
    assert_eq!(position, 67);
    assert_eq!(device.destinations.lock().len(), reads_before_retry + 1);
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
