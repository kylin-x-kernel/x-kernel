// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants with synthetic metadata; no host tools or embedded images.

use alloc::format;

use unittest::def_test;

use super::*;
use crate::{journal::RecoveryFlagPolicy, superblock::test_fixtures::*};

// Populate a real full directory while separating its data blocks physically.
// The filler names are hard links to one live inode, keeping the inode count
// small enough for the existing journal fixture even at external-leaf capacity.
fn fragmented_linear_directory_mutation(external_leaf: bool, rename: bool) {
    let (mut filesystem, device) = recovery_test_filesystem_with_free_blocks(900);
    let root = filesystem.internal_iget(InodeNumber::new(2)).unwrap();
    let timestamp = Ext4Timestamp::new(147, 0);
    let target = filesystem
        .create_directory(&root, b"fragmented", 0o755, 0, 0, timestamp)
        .unwrap();
    for index in 0..6 {
        filesystem
            .create_regular_file(
                &root,
                format!("padding-{index}").as_bytes(),
                0o644,
                0,
                0,
                timestamp,
            )
            .unwrap();
    }
    let moved = filesystem
        .create_regular_file(&root, b"pending", 0o644, 0, 0, timestamp)
        .unwrap();
    assert_ne!(
        filesystem.inode_table_entry_block(target.number()).unwrap(),
        filesystem.inode_table_entry_block(moved.number()).unwrap()
    );
    let blocks = if external_leaf {
        usize::from(crate::disk::extent::extent_block_capacity(TEST_BLOCK_SIZE).unwrap())
    } else {
        usize::from(crate::disk::extent::inline_extent_capacity().unwrap())
    };
    for logical in 0..blocks {
        let journal = filesystem
            .namei_metadata_journal(JournalCredits::new(64))
            .unwrap();
        let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
        let physical = if logical == 0 {
            filesystem.mapped_directory_block(&target, 0).unwrap()
        } else {
            let block = filesystem
                .allocate_block(
                    Some(FilesystemBlock::new(1040 + logical as u64 * 2)),
                    &mut handle,
                )
                .unwrap()
                .block();
            filesystem
                .insert_extent_mapping(
                    &target,
                    LogicalBlock::new(logical as u64),
                    block,
                    BlockCount::new(1),
                    ExtentMappingState::Initialized,
                    &mut handle,
                )
                .unwrap();
            block
        };
        let access = filesystem
            .metadata_io
            .write_access(FilesystemBlock::new(physical.get()), &mut handle)
            .unwrap();
        let mut bytes = vec![0; TEST_BLOCK_SIZE];
        let mut offset = 0;
        if logical == 0 {
            for (inode, name) in [
                (target.number(), b".".as_slice()),
                (root.number(), b"..".as_slice()),
            ] {
                write_dirent(
                    &mut bytes,
                    offset,
                    12,
                    TEST_BLOCK_SIZE,
                    inode,
                    DirectoryFileType::Directory,
                    name,
                )
                .unwrap();
                offset += 12;
            }
        }
        for index in 0..15 {
            let name = format!("{logical:03}{index:03}{}", "x".repeat(244));
            let len = if index == 14 {
                TEST_BLOCK_SIZE - offset
            } else {
                260
            };
            write_dirent(
                &mut bytes,
                offset,
                len,
                TEST_BLOCK_SIZE,
                moved.number(),
                DirectoryFileType::RegularFile,
                name.as_bytes(),
            )
            .unwrap();
            offset += len;
        }
        replace_metadata_access_bytes(&access, bytes).unwrap();
        filesystem
            .update_inode_size_metadata(
                &target,
                (logical as u64 + 1) * TEST_BLOCK_SIZE as u64,
                timestamp,
                &mut handle,
            )
            .unwrap();
        update_test_inode_links_count(
            &mut filesystem,
            &moved,
            ((logical + 1) * 15 + 1) as u16,
            &mut handle,
        );
        filesystem
            .complete_metadata_mutation(handle, Ok(()))
            .unwrap();
        // Bound setup transactions independently of the operation under test.
        filesystem.sync_filesystem().unwrap();
    }
    let header = crate::disk::extent::ExtentHeader::decode(&target.raw_i_block()).unwrap();
    assert_eq!(header.depth(), u16::from(external_leaf));
    for logical in 0..blocks {
        assert!(
            matches!(filesystem.map_blocks(&target, LogicalBlock::new(logical as u64)), Ok(BlockMapping::Mapped { len, .. }) if len == BlockCount::new(1))
        );
    }
    let name = "z".repeat(250);
    let plan = filesystem
        .preflight_directory_entry_insert(&target, name.as_bytes())
        .unwrap();
    assert_eq!(plan.path, DirectoryInsertPath::LinearAppend);
    assert!(
        plan.required_blocks > 1,
        "fixture must require extent metadata blocks"
    );
    let credits = if rename {
        Ext4Credits::rename(&filesystem, &root, &target, &moved, None, Some(plan))
    } else {
        Ext4Credits::link(&filesystem, &target, &moved, plan)
    };
    crate::journal::tests::limit_transaction_credits(&mut filesystem, credits.get());
    if rename {
        filesystem
            .rename(
                &root,
                b"pending",
                &moved,
                &target,
                name.as_bytes(),
                None,
                timestamp,
            )
            .unwrap();
        assert!(filesystem.lookup(&root, "pending").unwrap().is_none());
    } else {
        filesystem
            .link(&target, name.as_bytes(), &moved, timestamp)
            .unwrap();
        assert!(filesystem.lookup(&root, "pending").unwrap().is_some());
    }
    assert_eq!(
        filesystem.lookup(&target, &name).unwrap().unwrap().inode(),
        moved.number()
    );
    filesystem.sync_filesystem().unwrap();
    drop(filesystem);
    let reopened = open_test_filesystem(crate::io::test_block_device(device)).unwrap();
    let target = reopened.internal_iget(target.number()).unwrap();
    assert_eq!(
        reopened.lookup(&target, &name).unwrap().unwrap().inode(),
        moved.number()
    );
}

#[def_test]
fn fragmented_linear_directory_link_reserves_extent_split_credits() {
    for external_leaf in [false, true] {
        fragmented_linear_directory_mutation(external_leaf, false);
    }
}

#[def_test]
fn fragmented_linear_directory_rename_reserves_extent_split_credits() {
    for external_leaf in [false, true] {
        fragmented_linear_directory_mutation(external_leaf, true);
    }
}

#[def_test]
fn rename_into_full_linear_directory_reserves_growth_credits() {
    let (mut filesystem, device) = recovery_test_filesystem();
    let root = filesystem
        .internal_iget(crate::InodeNumber::new(2))
        .unwrap();
    let timestamp = crate::Ext4Timestamp::new(145, 0);
    let source = filesystem
        .create_directory(&root, b"source-dir", 0o755, 0, 0, timestamp)
        .unwrap();
    let target = filesystem
        .create_directory(&root, b"target-dir", 0o755, 0, 0, timestamp)
        .unwrap();
    let moved = filesystem
        .create_regular_file(&source, b"pending", 0o644, 0, 0, timestamp)
        .unwrap();
    for index in 0..15 {
        let name = format!("{index:03}{}", "x".repeat(247));
        filesystem
            .create_regular_file(&target, name.as_bytes(), 0o644, 0, 0, timestamp)
            .unwrap();
    }
    let destination = "y".repeat(250);
    let plan = filesystem
        .preflight_directory_entry_insert(&target, destination.as_bytes())
        .unwrap();
    assert_eq!(plan.path, DirectoryInsertPath::LinearAppend);
    assert!(plan.required_blocks > 0);
    // A fresh transaction prevents earlier creates from donating metadata credits.
    filesystem.sync_filesystem().unwrap();
    filesystem
        .rename(
            &source,
            b"pending",
            &moved,
            &target,
            destination.as_bytes(),
            None,
            timestamp,
        )
        .unwrap();
    assert!(filesystem.lookup(&source, "pending").unwrap().is_none());
    assert_eq!(
        filesystem
            .lookup(&target, &destination)
            .unwrap()
            .unwrap()
            .inode(),
        moved.number()
    );
    filesystem.sync_filesystem().unwrap();
    drop(filesystem);
    let reopened = open_test_filesystem(crate::io::test_block_device(device)).unwrap();
    let target = reopened.internal_iget(target.number()).unwrap();
    assert_eq!(
        reopened
            .lookup(&target, &destination)
            .unwrap()
            .unwrap()
            .inode(),
        moved.number()
    );
}

#[def_test]
fn regular_create_fits_parent_growth_journal_budget() {
    for (entries, credits, expected_path) in [
        (15, 14, DirectoryInsertPath::LinearAppend),
        (0, 10, DirectoryInsertPath::InPlace),
    ] {
        let (mut filesystem, device) = recovery_test_filesystem();
        let root = filesystem
            .internal_iget(crate::InodeNumber::new(2))
            .unwrap();
        let timestamp = crate::Ext4Timestamp::new(146, 0);
        let target = filesystem
            .create_directory(&root, b"small-journal-dir", 0o755, 0, 0, timestamp)
            .unwrap();
        for index in 0..entries {
            let name = format!("{index:03}{}", "x".repeat(247));
            filesystem
                .create_regular_file(&target, name.as_bytes(), 0o644, 0, 0, timestamp)
                .unwrap();
        }
        let name = "y".repeat(250);
        let plan = filesystem
            .preflight_directory_entry_insert(&target, name.as_bytes())
            .unwrap();
        assert_eq!(plan.path, expected_path);
        // Bound this test-owned transaction engine after setup, retaining
        // the valid 1024-block journal. Fourteen credits cover growth; ten
        // cover an in-place insert. A regular child allocates no data block.
        filesystem.sync_filesystem().unwrap();
        crate::journal::tests::limit_transaction_credits(&mut filesystem, credits);

        let child = filesystem
            .create_regular_file(&target, name.as_bytes(), 0o644, 0, 0, timestamp)
            .expect("regular child must fit the parent insertion budget");
        assert_eq!(
            filesystem.lookup(&target, &name).unwrap().unwrap().inode(),
            child.number()
        );
        filesystem.sync_filesystem().unwrap();
        drop(filesystem);
        let reopened = open_test_filesystem(crate::io::test_block_device(device)).unwrap();
        let target = reopened.internal_iget(target.number()).unwrap();
        assert_eq!(
            reopened.lookup(&target, &name).unwrap().unwrap().inode(),
            child.number()
        );
    }
}

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
