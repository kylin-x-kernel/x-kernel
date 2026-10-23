// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use unittest::def_test;

use crate::{
    Ext4InodeMetadataUpdate, Ext4XattrNamespace,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    superblock::test_fixtures::{
        allocate_checkpointed_directory_inode, install_test_internal_journal,
        journal_allocator_test_filesystem, update_test_inode_extra_isize,
    },
};

#[def_test]
fn xattr_mutation_defers_extra_isize_expansion_to_inode_dirty() {
    let (mut filesystem, _device) = journal_allocator_test_filesystem(512, 0b0011_1111);
    let inode = allocate_checkpointed_directory_inode(&mut filesystem);
    let journal = JournalTransactions::new(TransactionId::new(692));
    let mut handle = journal.begin(JournalCredits::new(2)).unwrap();
    let transaction = handle.id();
    update_test_inode_extra_isize(&mut filesystem, &inode, 0, &mut handle);
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    install_test_internal_journal(&mut filesystem, 692);
    filesystem
        .set_xattr(
            &inode,
            Ext4XattrNamespace::User,
            b"key",
            b"value",
            crate::Ext4Timestamp::new(692, 0),
        )
        .unwrap();
    assert_eq!(inode.extra_isize(), 0);

    filesystem
        .update_inode_metadata(
            &inode,
            Ext4InodeMetadataUpdate::new(crate::Ext4Timestamp::new(693, 0)).with_mode(0o700),
        )
        .unwrap();

    assert_eq!(
        inode.extra_isize(),
        filesystem.superblock().want_extra_isize()
    );
    assert_eq!(
        filesystem.raw_inode(inode.number()).unwrap().extra_isize(),
        filesystem.superblock().want_extra_isize()
    );
}

fn synchronous_xattr_image() -> alloc::vec::Vec<u8> {
    use crate::superblock::test_fixtures::*;
    let (mut filesystem, device) = recovery_test_filesystem();
    let root = filesystem
        .internal_iget(crate::InodeNumber::new(2))
        .unwrap();
    filesystem
        .create_regular_file(&root, b"file", 0o644, 0, 0, crate::Ext4Timestamp::new(1, 0))
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    let block = filesystem
        .map_journal_block(crate::jbd2::JournalBlock::new(0))
        .unwrap();
    let mut bytes = device.bytes();
    // v1 deliberately selects synchronous commit/checkpoint for fault timing.
    let journal = test_journal_superblock_bytes_with_blocks(700, 1024);
    let start = block.get() as usize * TEST_BLOCK_SIZE;
    bytes[start..start + journal.len()].copy_from_slice(&journal);
    bytes
}

fn xattr_file(filesystem: &crate::Ext4SbInfo) -> crate::Ext4Inode {
    let root = filesystem
        .internal_iget(crate::InodeNumber::new(2))
        .unwrap();
    filesystem
        .load_inode_private(filesystem.lookup(&root, "file").unwrap().unwrap().inode())
        .unwrap()
}

#[def_test]
fn inline_and_external_xattr_persist_noop_and_opaque_acl_accounting() {
    use alloc::{sync::Arc, vec};

    use crate::{Ext4Timestamp, superblock::test_fixtures::*};
    let device = Arc::new(TestDevice::new(synchronous_xattr_image()));
    let mut filesystem =
        open_test_filesystem(crate::io::test_block_device(device.clone())).unwrap();
    let inode = xattr_file(&filesystem);
    let original_blocks = inode.blocks();
    filesystem
        .set_xattr(
            &inode,
            Ext4XattrNamespace::User,
            b"inline",
            b"value",
            Ext4Timestamp::new(2, 0),
        )
        .unwrap();
    let committed = device.bytes();
    let ctime = inode.ctime();
    filesystem
        .set_xattr(
            &inode,
            Ext4XattrNamespace::User,
            b"inline",
            b"value",
            Ext4Timestamp::new(3, 0),
        )
        .unwrap();
    assert_eq!(inode.ctime(), ctime);
    assert_eq!(device.bytes(), committed);
    drop(filesystem);
    let mut filesystem =
        open_test_filesystem(crate::io::test_block_device(device.clone())).unwrap();
    let inode = xattr_file(&filesystem);
    assert_eq!(
        filesystem
            .get_xattr(&inode, Ext4XattrNamespace::User, b"inline")
            .unwrap()
            .as_deref(),
        Some(b"value".as_slice())
    );
    filesystem
        .remove_xattr(
            &inode,
            Ext4XattrNamespace::User,
            b"inline",
            Ext4Timestamp::new(4, 0),
        )
        .unwrap();
    let value: alloc::vec::Vec<_> = (0u8..=250).cycle().take(603).collect();
    let acl = [2, 0, 0, 0]; // Opaque storage contract; not a valid userspace POSIX ACL.
    filesystem
        .set_xattr(
            &inode,
            Ext4XattrNamespace::User,
            b"large",
            &value,
            Ext4Timestamp::new(5, 0),
        )
        .unwrap();
    assert_eq!(
        inode.blocks(),
        original_blocks + (TEST_BLOCK_SIZE / 512) as u64
    );
    filesystem
        .set_xattr(
            &inode,
            Ext4XattrNamespace::PosixAclAccess,
            b"",
            &acl,
            Ext4Timestamp::new(6, 0),
        )
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    drop(filesystem);
    let mut filesystem =
        open_test_filesystem(crate::io::test_block_device(device.clone())).unwrap();
    let inode = xattr_file(&filesystem);
    assert!(
        filesystem
            .get_xattr(&inode, Ext4XattrNamespace::User, b"inline")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        filesystem
            .get_xattr(&inode, Ext4XattrNamespace::User, b"large")
            .unwrap(),
        Some(value)
    );
    assert_eq!(
        filesystem
            .get_xattr(&inode, Ext4XattrNamespace::PosixAclAccess, b"")
            .unwrap(),
        Some(vec![2, 0, 0, 0])
    );
    assert_eq!(
        inode.blocks(),
        original_blocks + (TEST_BLOCK_SIZE / 512) as u64
    );
    filesystem
        .remove_xattr(
            &inode,
            Ext4XattrNamespace::User,
            b"large",
            Ext4Timestamp::new(7, 0),
        )
        .unwrap();
    assert_eq!(inode.blocks(), original_blocks);
    assert_eq!(
        filesystem
            .get_xattr(&inode, Ext4XattrNamespace::PosixAclAccess, b"")
            .unwrap()
            .as_deref(),
        Some(acl.as_slice())
    );
    filesystem
        .remove_xattr(
            &inode,
            Ext4XattrNamespace::PosixAclAccess,
            b"",
            Ext4Timestamp::new(8, 0),
        )
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    drop(filesystem);
    let filesystem = open_test_filesystem(crate::io::test_block_device(device)).unwrap();
    let inode = xattr_file(&filesystem);
    assert_eq!(inode.blocks(), original_blocks);
    assert!(filesystem.read_xattrs(&inode).unwrap().is_empty());
}

#[def_test]
fn external_xattr_write_and_flush_failures_preserve_old_state_and_allow_retry() {
    use alloc::{sync::Arc, vec};

    use crate::{
        Ext4Error, Ext4Timestamp,
        superblock::{fault_device::BufferedFaultDevice, test_fixtures::*},
    };
    let original = synchronous_xattr_image();
    let value = vec![b'f'; 600];
    for device in [
        Arc::new(BufferedFaultDevice::fail_write_at(original.clone(), 1)),
        Arc::new(BufferedFaultDevice::fail_flush_at(original.clone(), 1)),
    ] {
        let mut filesystem =
            open_test_filesystem(crate::io::test_block_device(device.clone())).unwrap();
        let inode = xattr_file(&filesystem);
        assert!(matches!(
            filesystem.set_xattr(
                &inode,
                Ext4XattrNamespace::User,
                b"large",
                &value,
                Ext4Timestamp::new(9, 0)
            ),
            Err(Ext4Error::Device(block::DriverError::Io) | Ext4Error::JournalAborted)
        ));
        drop(filesystem);
        let retry = Arc::new(TestDevice::new(device.committed_bytes()));
        let mut filesystem = open_test_filesystem(crate::io::test_block_device(retry)).unwrap();
        let inode = xattr_file(&filesystem);
        assert!(
            filesystem
                .get_xattr(&inode, Ext4XattrNamespace::User, b"large")
                .unwrap()
                .is_none()
        );
        filesystem
            .set_xattr(
                &inode,
                Ext4XattrNamespace::User,
                b"large",
                &value,
                Ext4Timestamp::new(10, 0),
            )
            .unwrap();
        assert_eq!(
            filesystem
                .get_xattr(&inode, Ext4XattrNamespace::User, b"large")
                .unwrap()
                .as_deref(),
            Some(value.as_slice())
        );
    }
}

#[def_test]
fn corrupt_external_xattr_checksum_identifies_inode_and_physical_block() {
    use super::*;
    use crate::superblock::test_fixtures::*;
    let (mut filesystem, _) = allocator_test_filesystem_with_options(
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0b0011_1111, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        true,
        false,
        false,
    );
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let block = 42;
    let mut bytes = alloc::vec![0; 4096];
    encode_external_xattr_block(
        &[Ext4Xattr {
            namespace: Ext4XattrNamespace::User,
            name: b"large".to_vec(),
            value: alloc::vec![b'z'; 900],
        }],
        block,
        filesystem.superblock().checksum_seed(),
        true,
        &mut bytes,
    )
    .unwrap();
    let header = disk_xattr::XattrBlockHeader::decode(&bytes).unwrap();
    filesystem
        .verify_xattr_block_checksum(&inode, block, &bytes, header)
        .unwrap();
    bytes[4095] ^= 1;
    assert!(matches!(
        filesystem.verify_xattr_block_checksum(&inode, block, &bytes, header),
        Err(Ext4Error::ChecksumMismatch {
            target: ChecksumTarget::XattrBlock { inode: number, block: physical }, ..
        }) if number == inode.number().get() && physical == block
    ));
}
