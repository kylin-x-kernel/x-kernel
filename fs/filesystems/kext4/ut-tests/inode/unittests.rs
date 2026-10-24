// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec::Vec;

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn extent_status_merges_written_ranges_at_maximum_block_boundary() {
    let mut cache = ExtentStatusTree::default();
    cache.cache_written(u64::MAX - 2, u64::MAX);
    cache.cache_written(u64::MAX - 6, u64::MAX - 4);
    assert!(!cache.contains_written(u64::MAX - 6, u64::MAX));
    cache.cache_written(u64::MAX - 5, u64::MAX - 1);
    assert!(cache.contains_written(u64::MAX - 6, u64::MAX));
    assert_eq!(cache.ranges.len(), 1);
    assert_eq!(cache.ranges[&(u64::MAX - 6)].len, 6);
    cache.cache_written(0, u64::MAX);
    assert!(cache.contains_written(0, u64::MAX));
    assert_eq!(cache.ranges[&0].len, u64::MAX);
    cache.cache_written(9, 8);
    cache.cache_written(u64::MAX, u64::MAX);
    assert_eq!(cache.ranges.len(), 1);
}

#[def_test]
fn initialized_ranges_cover_random_overwrites_without_bridging_holes() {
    let mut metadata = regular_inode_metadata();
    metadata.disk_size = 8 * 4096;
    let inode = test_inode(metadata);
    inode.cache_initialized_blocks(0, 3);
    inode.cache_initialized_blocks(4, 8);
    for block in [7, 1, 4, 0, 6, 2, 5] {
        assert!(inode.has_initialized_overwrite(block, 1, 4096));
    }
    assert!(!inode.has_initialized_overwrite(3, 1, 4096));
    assert!(!inode.has_initialized_overwrite(2, 3, 4096));
    inode.cache_initialized_blocks(3, 4);
    assert!(inode.has_initialized_overwrite(0, 8, 4096));
}

#[def_test]
fn initialized_overwrites_exclude_extensions_and_pending_reservations() {
    let mut metadata = regular_inode_metadata();
    metadata.disk_size = 2 * 4096 + 1;
    let inode = test_inode(metadata);
    inode.cache_initialized_blocks(0, 8);
    assert!(inode.has_initialized_overwrite(0, 2, 4096));
    assert!(!inode.has_initialized_overwrite(2, 1, 4096));
    assert!(!inode.has_initialized_overwrite(0, 0, 4096));
    assert!(!inode.has_initialized_overwrite(u64::MAX, 2, 4096));
    inode.set_size(8 * 4096);
    assert!(!inode.has_initialized_overwrite(2, 1, 4096));
    inode.insert_unreserved_delalloc_extents(&[(6, 7)]).unwrap();
    assert!(!inode.has_initialized_overwrite(0, 1, 4096));
    inode.remove_delalloc_from(6).unwrap();
    assert!(inode.has_initialized_overwrite(0, 1, 4096));
}

#[def_test]
fn initialized_overwrites_are_invalidated_by_shrink_and_mapping_changes() {
    let mut metadata = regular_inode_metadata();
    metadata.disk_size = 4 * 4096;
    let inode = test_inode(metadata);
    inode.cache_initialized_blocks(0, 4);
    inode.set_size(4096);
    inode.set_size(4 * 4096);
    assert!(!inode.has_initialized_overwrite(0, 1, 4096));

    inode.cache_initialized_blocks(0, 4);
    inode.invalidate_extent_status();
    assert!(!inode.has_initialized_overwrite(0, 1, 4096));

    inode.cache_initialized_blocks(0, 4);
    let mut metadata = inode.metadata_snapshot();
    metadata.block[0] = 1;
    inode.publish_metadata(metadata).unwrap();
    assert!(!inode.has_initialized_overwrite(0, 1, 4096));

    inode.cache_initialized_blocks(0, 4);
    let mut metadata = inode.metadata_snapshot();
    metadata.disk_size = 4096;
    inode.publish_metadata(metadata).unwrap();
    assert!(!inode.has_initialized_overwrite(0, 1, 4096));
}

#[def_test]
fn initialized_overwrites_survive_timestamp_updates() {
    let mut metadata = regular_inode_metadata();
    metadata.disk_size = 4 * 4096;
    let inode = test_inode(metadata);
    inode.cache_initialized_blocks(0, 4);
    let mut metadata = inode.metadata_snapshot();
    metadata.mtime = timestamp(123, 0);
    inode.publish_metadata(metadata).unwrap();
    assert!(inode.has_initialized_overwrite(0, 4, 4096));
}

#[def_test]
fn initialized_range_cache_is_bounded_and_eviction_requires_revalidation() {
    let mut cache = ExtentStatusTree::default();
    for index in 0..ExtentStatusTree::MAX_RANGES as u64 {
        cache.cache_written(index * 2, index * 2 + 1);
    }
    assert!(cache.contains_written(0, 1));
    let last = ExtentStatusTree::MAX_RANGES as u64 * 2;
    cache.cache_written(last, last + 1);
    assert!(!cache.contains_written(0, 1));
    assert!(cache.contains_written(last, last + 1));
    // Eviction must preserve the other ranges instead of clearing the tree.
    assert!(cache.contains_written(4, 5));
    assert!(cache.ranges.len() <= ExtentStatusTree::MAX_RANGES);
    cache.cache_written(0, 1);
    assert!(cache.contains_written(0, 1));
}

#[def_test]
fn eviction_prefers_the_shortest_range_over_the_lowest_offset() {
    let mut cache = ExtentStatusTree::default();
    // Fill to capacity with equal-length ranges at low offsets, then add one
    // strictly shorter range at a high offset so the candidate policies
    // ("shortest range" vs "lowest offset") pick different victims.
    for index in 0..(ExtentStatusTree::MAX_RANGES as u64 - 1) {
        let start = index * 4;
        cache.cache_written(start, start + 2);
    }
    cache.cache_written(0x4000, 0x4001);
    assert_eq!(cache.ranges.len(), ExtentStatusTree::MAX_RANGES);

    // The next insertion must evict the shortest range, not the lowest offset.
    cache.cache_written(0x8000, 0x8002);

    assert!(!cache.contains_written(0x4000, 0x4001));
    assert!(cache.contains_written(0, 2));
    assert!(cache.contains_written(0x8000, 0x8002));
    assert_eq!(cache.ranges.len(), ExtentStatusTree::MAX_RANGES);
}

fn test_inode(metadata: Ext4InodeMetadata) -> Ext4Inode {
    Ext4Inode::new(InodeNumber::new(12), metadata)
}

fn regular_inode_metadata() -> Ext4InodeMetadata {
    Ext4InodeMetadata {
        kind: InodeKind::RegularFile,
        mode: disk_inode::S_IFREG | 0o644,
        uid: 0,
        gid: 0,
        disk_size: 0,
        blocks: 0,
        flags: disk_inode::EXT4_EXTENTS_FL,
        block: [0; disk_inode::INODE_BLOCK_BYTES],
        file_acl: 0,
        extra_isize: 0,
        inline_xattr: alloc::vec::Vec::new(),
        generation: 1,
        links_count: 1,
        atime: timestamp(0, 0),
        ctime: timestamp(0, 0),
        mtime: timestamp(0, 0),
    }
}

const fn timestamp(seconds: i64, nanos: u32) -> Ext4Timestamp {
    Ext4Timestamp::new(seconds, nanos)
}

#[def_test]
fn orphan_truncation_classifies_inode_kind_and_symlink_storage() {
    for (kind, expected) in [
        (InodeKind::RegularFile, true),
        (InodeKind::Directory, true),
        (InodeKind::Fifo, false),
        (InodeKind::Socket, false),
        (InodeKind::CharacterDevice, false),
        (InodeKind::BlockDevice, false),
    ] {
        let mut metadata = regular_inode_metadata();
        metadata.kind = kind;
        assert_eq!(test_inode(metadata).can_truncate(4096, false), Ok(expected));
    }
    let mut metadata = regular_inode_metadata();
    metadata.kind = InodeKind::Symlink;
    metadata.disk_size = 9;
    metadata.file_acl = 1;
    metadata.blocks = 8;
    let inode = test_inode(metadata);
    assert_eq!(inode.can_truncate(4096, false), Ok(false));
    inode.update_state(|state| state.metadata.blocks += 8);
    assert_eq!(inode.can_truncate(4096, false), Ok(true));
    assert_eq!(inode.can_truncate(4096, true), Ok(false));
}

#[def_test]
fn timestamp_limits_track_inode_extra_field_capacity() {
    let legacy = timestamp_limits_for_inode_size(128);
    assert_eq!(legacy.granularity_ns(), ktime_types::NANOS_PER_SEC as u32);
    assert_eq!(legacy.minimum_seconds(), i32::MIN as i64);
    assert_eq!(legacy.maximum_seconds(), i32::MAX as i64);

    let extended = timestamp_limits_for_inode_size(256);
    assert_eq!(extended.granularity_ns(), 1);
    assert_eq!(extended.minimum_seconds(), i32::MIN as i64);
    assert_eq!(extended.maximum_seconds(), 15_032_385_535);
}

#[def_test]
fn initialized_extended_inode_preserves_epoch_and_nanoseconds() {
    let timestamp = Ext4Timestamp::new(2_147_483_648, 123_456_789);
    let mut bytes = [0; 256];
    encode_initialized_inode(
        &mut bytes,
        InodeNumber::new(11),
        InodeInitialization::regular_file(0o644, 1000, 1001).with_timestamp(timestamp),
        32,
        false,
        false,
        0,
    )
    .unwrap();

    let metadata =
        Ext4InodeMetadata::from_raw(disk_inode::RawInode::decode(&bytes).unwrap(), false).unwrap();
    assert_eq!(metadata.atime, timestamp);
    assert_eq!(metadata.ctime, timestamp);
    assert_eq!(metadata.mtime, timestamp);
}

#[def_test]
fn initialized_legacy_inode_clamps_post_2038_timestamp() {
    let mut bytes = [0; disk_inode::GOOD_OLD_INODE_SIZE];
    encode_initialized_inode(
        &mut bytes,
        InodeNumber::new(11),
        InodeInitialization::regular_file(0o644, 1000, 1001)
            .with_timestamp(Ext4Timestamp::new(2_147_483_648, 123_456_789)),
        0,
        false,
        false,
        0,
    )
    .unwrap();

    let metadata =
        Ext4InodeMetadata::from_raw(disk_inode::RawInode::decode(&bytes).unwrap(), false).unwrap();
    let maximum = Ext4Timestamp::new(i32::MAX as i64, 0);
    assert_eq!(metadata.atime, maximum);
    assert_eq!(metadata.ctime, maximum);
    assert_eq!(metadata.mtime, maximum);
}

#[def_test]
fn generic_size_publication_is_shared_with_ext4_without_changing_disk_size() {
    let mut metadata = regular_inode_metadata();
    metadata.disk_size = 2048;
    let inode = test_inode(metadata);
    let attributes: &dyn kvfs::InodeAttributeOperations = &inode;

    attributes.set_size(4096);
    assert_eq!(inode.size(), 4096);
    assert_eq!(inode.sizes(), (4096, 2048));
    assert_eq!(inode.stat().size, 4096);

    inode.set_size(1024);
    assert_eq!(attributes.size(), 1024);
    assert_eq!(
        attributes
            .fill_metadata(u64::from(inode.number().get()), 4096)
            .size,
        1024
    );
    assert_eq!(inode.sizes(), (1024, 2048));
}

#[def_test]
fn visible_size_read_does_not_acquire_inode_mapping_state() {
    let inode = test_inode(regular_inode_metadata());
    inode.set_size(4096);
    let _state = inode.state.write();

    assert_eq!(inode.size(), 4096);
    assert_eq!(kvfs::InodeAttributeOperations::size(&inode), 4096);
}

#[def_test]
fn inode_state_accessors_reflect_metadata_fields() {
    let mut metadata = regular_inode_metadata();
    metadata.uid = 7;
    metadata.gid = 9;
    metadata.flags = disk_inode::EXT4_IMMUTABLE_FL | disk_inode::EXT4_APPEND_FL;
    let inode = test_inode(metadata);

    inode.set_size(100);
    inode.update_state(|state| state.metadata.disk_size = 200);

    let (uid, gid, size, disk_size, immutable, append_only) = inode.with_state(|state| {
        (
            state.uid(),
            state.gid(),
            inode.size(),
            state.disk_size(),
            state.is_immutable(),
            state.is_append_only(),
        )
    });
    assert_eq!(uid, 7);
    assert_eq!(gid, 9);
    assert_eq!(size, 100);
    assert_eq!(disk_size, 200);
    assert!(immutable);
    assert!(append_only);
}

#[def_test]
fn update_state_publishes_mutation_to_later_readers() {
    let inode = test_inode(regular_inode_metadata());

    inode.set_size(1234);
    inode.update_state(|state| state.metadata.disk_size = 5678);
    inode.update_state(|state| state.metadata.uid = 42);

    let (size, disk_size, uid) =
        inode.with_state(|state| (inode.size(), state.disk_size(), state.uid()));
    assert_eq!(size, 1234);
    assert_eq!(disk_size, 5678);
    assert_eq!(uid, 42);
}

/// The three tests below exercise real `RwLock` behavior and therefore
/// need the kernel scheduler (`ktask`) and kernel sync primitives
/// (`ksync`), which only exist when building for the kernel
/// (`target_os = "none"`). `make UNITTEST=y` builds exactly that target.
#[cfg(target_os = "none")]
mod concurrent {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use ksync::Semaphore;
    use ktask::spawn;
    use unittest::{assert, assert_eq, def_test};

    use super::*;

    #[def_test]
    fn with_state_allows_concurrent_readers() {
        const READER_COUNT: usize = 4;

        let inode = Arc::new(test_inode(regular_inode_metadata()));
        let gate = Arc::new(Semaphore::new(0));
        let entered = Arc::new(AtomicUsize::new(0));
        let max_active_readers = Arc::new(AtomicUsize::new(0));
        let release_all = Arc::new(AtomicBool::new(false));

        let mut readers = Vec::new();
        for _ in 0..READER_COUNT {
            let inode = Arc::clone(&inode);
            let gate = Arc::clone(&gate);
            let entered = Arc::clone(&entered);
            let max_active_readers = Arc::clone(&max_active_readers);
            let release_all = Arc::clone(&release_all);
            readers.push(spawn(move || {
                gate.acquire();
                inode.with_state(|_state| {
                    let active = entered.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active_readers.fetch_max(active, Ordering::SeqCst);
                    // Stay inside the read lock, spinning with a yield,
                    // until the main thread has observed that several
                    // readers hold the shared lock at the same time. This
                    // deterministically proves concurrent readers. Never
                    // block (deschedule) while holding the lock: a reader
                    // parked in a semaphore `block_on` races the
                    // Running -> Blocked transition against a cross-CPU
                    // wake, which can strand it in Blocked forever and
                    // hang the test (observed on SMP builds).
                    while !release_all.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                        ktask::yield_now();
                    }
                });
            }));
        }
        for _ in 0..READER_COUNT {
            gate.release();
        }
        // Wait until every reader is inside `with_state` and at least two
        // of them hold the read lock at once, then let all of them leave.
        while entered.load(Ordering::Acquire) < READER_COUNT
            || max_active_readers.load(Ordering::Acquire) < 2
        {
            ktask::yield_now();
        }
        release_all.store(true, Ordering::Release);
        for reader in readers {
            reader.join();
        }

        assert_eq!(entered.load(Ordering::SeqCst), READER_COUNT);
        assert!(
            max_active_readers.load(Ordering::SeqCst) > 1,
            "with_state must admit concurrent readers"
        );
    }

    #[def_test]
    fn update_state_serializes_concurrent_writers() {
        const WRITER_COUNT: usize = 4;

        let inode = Arc::new(test_inode(regular_inode_metadata()));
        let gate = Arc::new(Semaphore::new(0));
        let active_writers = Arc::new(AtomicUsize::new(0));
        let max_active_writers = Arc::new(AtomicUsize::new(0));

        let mut writers = Vec::new();
        for _ in 0..WRITER_COUNT {
            let inode = Arc::clone(&inode);
            let gate = Arc::clone(&gate);
            let active_writers = Arc::clone(&active_writers);
            let max_active_writers = Arc::clone(&max_active_writers);
            writers.push(spawn(move || {
                gate.acquire();
                inode.update_state(|state| {
                    state.metadata.disk_size += 1;
                    let active = active_writers.fetch_add(1, Ordering::SeqCst) + 1;
                    max_active_writers.fetch_max(active, Ordering::SeqCst);
                    ktask::yield_now();
                    active_writers.fetch_sub(1, Ordering::SeqCst);
                });
            }));
        }
        for _ in 0..WRITER_COUNT {
            gate.release();
        }
        for writer in writers {
            writer.join();
        }

        assert_eq!(
            max_active_writers.load(Ordering::SeqCst),
            1,
            "update_state must be write-exclusive"
        );
        assert_eq!(
            inode.disk_size(),
            WRITER_COUNT as u64,
            "serialized writers must not lose updates"
        );
    }

    #[def_test]
    fn shrink_publication_and_extent_proof_invalidation_are_serialized() {
        const WRITER_COUNT: usize = 1;
        const READER_COUNT: usize = 3;
        const ITERATIONS: u64 = 10_000;

        const GROWN_SIZE: u64 = 4 * 4096;
        const SHRUNK_SIZE: u64 = 4096;

        let mut metadata = regular_inode_metadata();
        metadata.disk_size = GROWN_SIZE;
        let inode = Arc::new(test_inode(metadata));
        let gate = Arc::new(Semaphore::new(0));
        let has_stale_proof = Arc::new(AtomicBool::new(false));

        let mut workers = Vec::new();
        for _ in 0..WRITER_COUNT {
            let inode = Arc::clone(&inode);
            let gate = Arc::clone(&gate);
            workers.push(spawn(move || {
                gate.acquire();
                for _ in 0..ITERATIONS {
                    inode.set_size(GROWN_SIZE);
                    inode.cache_initialized_blocks(0, 4);
                    inode.set_size(SHRUNK_SIZE);
                }
            }));
        }
        for _ in 0..READER_COUNT {
            let inode = Arc::clone(&inode);
            let gate = Arc::clone(&gate);
            let has_stale_proof = Arc::clone(&has_stale_proof);
            workers.push(spawn(move || {
                gate.acquire();
                for _ in 0..ITERATIONS {
                    inode.with_state(|state| {
                        // A mapping-state reader must not combine the new
                        // shorter EOF with proofs surviving from before shrink.
                        if inode.size() == SHRUNK_SIZE && state.extent_status.contains_written(0, 4)
                        {
                            has_stale_proof.store(true, Ordering::SeqCst);
                        }
                    });
                }
            }));
        }

        for _ in 0..(WRITER_COUNT + READER_COUNT) {
            gate.release();
        }
        for worker in workers {
            worker.join();
        }

        assert!(
            !has_stale_proof.load(Ordering::SeqCst),
            "shrink must invalidate extent proofs before publishing the shorter EOF"
        );
        assert_eq!(inode.sizes(), (SHRUNK_SIZE, GROWN_SIZE));
        assert!(!inode.has_initialized_overwrite(0, 1, 4096));
    }
}
