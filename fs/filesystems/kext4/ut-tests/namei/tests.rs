// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants with synthetic metadata; no host tools or embedded images.

use alloc::format;

use unittest::def_test;

use super::*;
use crate::{journal::RecoveryFlagPolicy, superblock::test_fixtures::*};

#[def_test]
fn namespace_removal_tracks_orphan_file_until_final_eviction() {
    let (mut filesystem, _device) = recovery_orphan_file_test_filesystem();
    let root = filesystem
        .internal_iget(crate::InodeNumber::new(2))
        .unwrap();
    let free_inodes = filesystem.free_inodes_count();
    let parent_links = root.links_count();
    let timestamp = crate::Ext4Timestamp::new(127, 0);
    let target = filesystem
        .create_regular_file(&root, b"target", 0o644, 0, 0, timestamp)
        .unwrap();
    let source = filesystem
        .create_regular_file(&root, b"source", 0o644, 0, 0, timestamp)
        .unwrap();
    let directory = filesystem
        .create_directory(&root, b"directory", 0o755, 0, 0, timestamp)
        .unwrap();

    filesystem
        .link(&root, b"alias", &target, timestamp)
        .unwrap();
    filesystem
        .unlink(&root, b"alias", &target, timestamp)
        .unwrap();
    assert_eq!(target.links_count(), 1);
    assert_eq!(target.orphan_location(), None);
    assert_eq!(filesystem.orphan_file_empty(), Ok(true));

    filesystem
        .rename(
            &root,
            b"source",
            &source,
            &root,
            b"target",
            Some(&target),
            timestamp,
        )
        .unwrap();
    filesystem
        .remove_directory(&root, b"directory", &directory, timestamp)
        .unwrap();
    filesystem
        .unlink(&root, b"target", &source, timestamp)
        .unwrap();
    for name in ["source", "target", "directory", "alias"] {
        assert!(filesystem.lookup(&root, name).unwrap().is_none());
    }
    assert_eq!(root.links_count(), parent_links);
    assert_eq!(filesystem.orphan_head(), None);
    for inode in [&target, &source, &directory] {
        assert_eq!(inode.links_count(), 0);
        assert!(filesystem.is_inode_allocated(inode.number()).unwrap());
        assert!(matches!(
            inode.orphan_location(),
            Some(crate::inode::OrphanLocation::File { .. })
        ));
    }
    filesystem.sync_filesystem().unwrap();
    assert_eq!(filesystem.orphan_file_empty(), Ok(false));

    // Eviction order differs from insertion order and includes a directory.
    for inode in [&directory, &target, &source] {
        filesystem
            .evict_unlinked_inode_with_policy(
                inode,
                timestamp,
                RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
            .unwrap();
        assert_eq!(inode.orphan_location(), None);
        assert!(!filesystem.is_inode_allocated(inode.number()).unwrap());
    }
    assert_eq!(filesystem.orphan_file_empty(), Ok(true));
    assert_eq!(filesystem.free_inodes_count(), free_inodes);
    filesystem.sync_filesystem().unwrap();
}

#[def_test]
fn symlink_delete_releases_data_block_through_metadata_forget() {
    // Deleting a block-mapped symlink must free its target block through
    // the journaled metadata release path (forget + revoke), not the plain
    // data path. The freed block must become immediately reusable, the
    // orphan list must drain. Disk-format interoperability runs in harness.
    let (mut filesystem, _device) = recovery_test_filesystem();
    let root = filesystem
        .internal_iget(crate::InodeNumber::new(2))
        .expect("read root inode");
    let timestamp = crate::Ext4Timestamp::new(144, 0);
    filesystem
        .metadata_journal()
        .expect("journaled test image has an internal journal");
    assert!(filesystem.journal_supports_revoke());
    let free_blocks_before = filesystem.free_blocks_count();
    let free_inodes_before = filesystem.free_inodes_count();

    let target = b"/opt/package/lib/libremoved.so.1.0.0-symlink-delete-forget-target-block";
    let symlink = filesystem
        .create_symlink(&root, b"removed-link", target, 0, 0, timestamp)
        .expect("create block-mapped symlink");
    assert_eq!(symlink.kind(), InodeKind::Symlink);
    assert_eq!(filesystem.free_blocks_count(), free_blocks_before - 1);

    filesystem
        .unlink(&root, b"removed-link", &symlink, timestamp)
        .expect("unlink block-mapped symlink");
    filesystem
        .evict_unlinked_inode_with_policy(
            &symlink,
            timestamp,
            RecoveryFlagPolicy::ClearAfterCheckpoint,
        )
        .expect("evict unlinked symlink");

    assert_eq!(
        filesystem
            .lookup(&root, "removed-link")
            .expect("lookup removed symlink"),
        None
    );
    assert_eq!(filesystem.orphan_head(), None);
    assert_eq!(filesystem.free_blocks_count(), free_blocks_before);
    assert_eq!(filesystem.free_inodes_count(), free_inodes_before);
    assert!(
        filesystem
            .metadata_journal()
            .expect("journal present")
            .begin(JournalCredits::new(1))
            .is_ok(),
        "metadata forget release aborted the journal"
    );

    // The forgotten block must be cleanly reusable right away.
    let replacement = filesystem
        .create_symlink(&root, b"replacement-link", target, 0, 0, timestamp)
        .expect("recreate symlink on forgotten block");
    assert_eq!(filesystem.free_blocks_count(), free_blocks_before - 1);
    let mut buffer = vec![0u8; target.len()];
    let read = filesystem
        .read_link_at(&replacement, 0, &mut buffer)
        .expect("read recreated symlink target");
    assert_eq!(read, target.len());
    assert_eq!(&buffer[..read], target);

    filesystem
        .sync_filesystem()
        .expect("sync journaled namespace image");
    drop(filesystem);
}

#[def_test]
fn symlink_create_delete_recreate_cycle_reads_current_target() {
    // Original bug regression: rapid create/delete/recreate of block-mapped
    // symlinks reuses the freed target block before the old journal
    // checkpoint completes. Without forget/revoke on the release path the
    // metadata cache could serve stale or zeroed target content. Every
    // iteration must read back exactly the target it created; the
    // allocator deterministically hands back the just-freed block (the
    // shortest free run is preferred for a one-block request), so that
    // read is the stale / zeroed-content check. The reuse premise itself
    // is asserted at the bottom of the test.
    let (mut filesystem, _device) = recovery_test_filesystem();
    let timestamp = crate::Ext4Timestamp::new(144, 0);
    filesystem
        .metadata_journal()
        .expect("journaled test image has an internal journal");
    assert!(filesystem.journal_supports_revoke());
    let free_blocks_before = filesystem.free_blocks_count();

    let mut previous_physical: Option<u64> = None;
    let mut reuse_iterations = 0u32;
    for iteration in 0..8u64 {
        let root = filesystem
            .internal_iget(crate::InodeNumber::new(2))
            .expect("read root inode");
        let target = format!(
            "/opt/package/lib/libcycle.so.{iteration}.0-symlink-recreate-target-block-padding"
        );
        let symlink = filesystem
            .create_symlink(&root, b"link", target.as_bytes(), 0, 0, timestamp)
            .expect("create block-mapped symlink");
        assert_eq!(symlink.kind(), InodeKind::Symlink);

        let mapping = filesystem
            .map_blocks(&symlink, LogicalBlock::new(0))
            .expect("map symlink target block");
        let BlockMapping::Mapped { physical, .. } = mapping else {
            panic!("expected mapped symlink target block");
        };
        if previous_physical.is_some_and(|previous| physical.get() == previous) {
            // The allocator reused the just-freed target block: the
            // original bug trigger. The read below must still serve the
            // freshly written target, which the per-iteration assertion
            // enforces.
            reuse_iterations += 1;
        }
        previous_physical = Some(physical.get());

        let mut buffer = vec![0u8; target.len()];
        let read = filesystem
            .read_link_at(&symlink, 0, &mut buffer)
            .expect("read symlink target");
        assert_eq!(read, target.len());
        assert_eq!(
            &buffer[..read],
            target.as_bytes(),
            "iteration {iteration} read a stale or zeroed target block"
        );

        filesystem
            .unlink(&root, b"link", &symlink, timestamp)
            .expect("unlink block-mapped symlink");
        filesystem
            .evict_unlinked_inode_with_policy(
                &symlink,
                timestamp,
                RecoveryFlagPolicy::ClearAfterCheckpoint,
            )
            .expect("evict unlinked symlink");
        assert_eq!(
            filesystem
                .lookup(&root, "link")
                .expect("lookup removed link"),
            None
        );
        assert_eq!(filesystem.orphan_head(), None);
        assert_eq!(
            filesystem.free_blocks_count(),
            free_blocks_before,
            "iteration {iteration} leaked the symlink target block"
        );
        assert!(
            filesystem
                .metadata_journal()
                .expect("journal present")
                .begin(JournalCredits::new(1))
                .is_ok(),
            "iteration {iteration} aborted the journal"
        );
    }
    // Reading the current target only exercises the stale-cache regression
    // if a freed block was actually reused. Keep that premise explicit when
    // changing the fixture or allocation policy.
    assert!(
        reuse_iterations > 0,
        "allocator never reused the just-freed symlink target block; the stale/zeroed metadata \
         regression scenario was not exercised"
    );

    filesystem
        .sync_filesystem()
        .expect("sync journaled namespace image");
    drop(filesystem);
}

#[def_test]
fn corrupt_htree_checksum_identifies_directory_and_logical_block() {
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
    let inode = allocate_checkpointed_directory_inode(&mut filesystem);
    let mut bytes = alloc::vec![0; 4096];
    bytes[32..34].copy_from_slice(&507u16.to_le_bytes());
    bytes[34..36].copy_from_slice(&1u16.to_le_bytes());
    let count_limit = disk_dir::HTreeCountLimit::decode(&bytes, 32).unwrap();
    filesystem
        .update_htree_block_checksum(&inode, &mut bytes, 32, count_limit)
        .unwrap();
    filesystem
        .verify_htree_block_checksum(&inode, 0, &bytes, 32, count_limit)
        .unwrap();
    bytes[4092] ^= 1;
    assert!(matches!(
        filesystem.verify_htree_block_checksum(&inode, 0, &bytes, 32, count_limit),
        Err(Ext4Error::ChecksumMismatch {
            target: crate::ChecksumTarget::DirectoryBlock { inode: number, block: 0 }, ..
        }) if number == inode.number().get()
    ));
}
