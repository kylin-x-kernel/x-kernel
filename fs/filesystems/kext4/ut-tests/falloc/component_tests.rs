// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Component invariants exercised in the kernel with synthetic metadata.

use alloc::{string::String, sync::Arc, vec};

use kvfs::{
    Dentry, FallocateFlags, Mount, OpenFlags, SuperBlock, SuperBlockFlags, VfsResult, dentry_open,
};
use unittest::def_test;

use crate::{
    BlockCount, BlockMapping, Ext4Inode, Ext4SyncIntent, FILE_SYSTEM_TYPE, FilesystemBlock,
    LogicalBlock,
    superblock::test_fixtures::{
        TEST_BLOCK_SIZE, TEST_FREE_BLOCKS, allocate_checkpointed_regular_inode, fallocate_vfs,
        install_test_internal_journal, journal_allocator_test_filesystem,
    },
    sync::Mutex,
    vfs::{EXT4_SUPER_OPERATIONS, Ext4Mount, Ext4SuperOperations},
};

/// KEEP_SIZE preallocates an unwritten extent without moving size or
/// `i_disksize`, and repeating the call allocates nothing.
#[def_test]
fn fallocate_keep_size_preallocates_unwritten_extent() {
    let fx = fallocate_vfs(TEST_FREE_BLOCKS, 0b0011_1111, 761);
    let timestamp = crate::Ext4Timestamp::new(30, 0);
    let free_before = fx.free_blocks();
    let len = (TEST_BLOCK_SIZE * 4) as u64;
    let mut committed = 0u64;

    fx.ext4()
        .fallocate_regular_inode_preallocate(
            fx.super_block.as_ref(),
            fx.inode(),
            0,
            len,
            true,
            timestamp,
            &mut committed,
        )
        .unwrap();

    assert_eq!(committed, 0);
    assert_eq!(fx.inode().size(), 0);
    assert_eq!(fx.inode().disk_size(), 0);
    assert_eq!(fx.inode().blocks(), (TEST_BLOCK_SIZE / 512 * 4) as u64);
    assert_eq!(fx.free_blocks(), free_before - 4);
    assert!(matches!(
        fx.ext4().read().map_blocks(fx.inode(), LogicalBlock::new(0)),
        Ok(BlockMapping::Unwritten { len, flags, .. })
            if len == BlockCount::new(4) && flags.is_empty()
    ));
    assert_eq!(fx.inode().ctime(), timestamp);
    assert_eq!(fx.inode().mtime(), timestamp);

    // Repeating the request must not allocate twice.
    let mut committed = 0u64;
    fx.ext4()
        .fallocate_regular_inode_preallocate(
            fx.super_block.as_ref(),
            fx.inode(),
            0,
            len,
            true,
            crate::Ext4Timestamp::new(31, 0),
            &mut committed,
        )
        .unwrap();
    assert_eq!(committed, 0);
    assert_eq!(fx.inode().blocks(), (TEST_BLOCK_SIZE / 512 * 4) as u64);
    assert_eq!(fx.free_blocks(), free_before - 4);
}

/// Non-KEEP_SIZE commits `i_disksize` and zeroes the old EOF tail while the
/// written prefix reads back unchanged.
#[def_test]
fn fallocate_extends_disk_size_and_zeroes_old_eof_tail() {
    let fx = fallocate_vfs(TEST_FREE_BLOCKS, 0b0011_1111, 762);
    let input = [0x6d; 23];
    let mut pos = 0;
    assert_eq!(fx.file.write_from(&input, &mut pos), Ok(input.len()));
    fx.file.fsync(false).unwrap();
    assert_eq!(fx.inode().disk_size(), 23);

    // The writeback `ZeroFill` path already zeroed [23, block_size), so inject
    // stale bytes there directly: otherwise the EOF-tail zeroing below is
    // unobservable and removing it would still pass.
    let BlockMapping::Mapped { physical, .. } =
        fx.ext4().read().map_blocks(fx.inode(), LogicalBlock::new(0)).unwrap()
    else {
        panic!("written prefix should be a mapped extent");
    };
    let mut stale = vec![0x6d; TEST_BLOCK_SIZE];
    stale[23..].fill(0xc7);
    fx.ext4()
        .read()
        .device
        .write_contiguous_blocks(FilesystemBlock::new(physical.get()), 1, &stale)
        .unwrap();

    let new_size = (TEST_BLOCK_SIZE * 3) as u64;
    // Non-KEEP_SIZE success must publish the visible size through the VFS exit.
    fx.file.fallocate(FallocateFlags::empty(), 0, new_size).unwrap();

    assert_eq!(fx.file.size(), new_size);
    assert_eq!(fx.inode().disk_size(), new_size);
    // Core `read_at` bypasses the page cache, so it observes the device bytes
    // the zeroing rewrites; widen the core visible size to read past old EOF.
    fx.inode().set_size(new_size);
    let mut output = vec![0; TEST_BLOCK_SIZE];
    assert_eq!(
        fx.ext4().read().read_at(fx.inode(), 0, &mut output).unwrap(),
        TEST_BLOCK_SIZE
    );
    assert_eq!(&output[..23], &[0x6d; 23]);
    assert!(output[23..].iter().all(|byte| *byte == 0));
    assert!(matches!(
        fx.ext4().read().map_blocks(fx.inode(), LogicalBlock::new(1)),
        Ok(BlockMapping::Unwritten { flags, .. }) if flags.is_empty()
    ));
}

/// Preallocation over a written prefix allocates only the hole tail and keeps
/// the written data.
#[def_test]
fn fallocate_skips_existing_mappings_and_keeps_data() {
    let fx = fallocate_vfs(TEST_FREE_BLOCKS, 0b0011_1111, 763);
    let inode = fx.inode_arc();
    let input = vec![0xa5; TEST_BLOCK_SIZE * 4];
    inode.set_size(input.len() as u64);
    fx.ext4()
        .write(fx.super_block.as_ref())
        .writeback_ordered_at_with_prealloc_budget(
            &inode,
            0,
            &input,
            input.len() as u64,
            Ext4SyncIntent::DataOnly,
            0,
            || true,
        )
        .unwrap();
    let blocks_before = inode.blocks();
    assert_eq!(blocks_before, (TEST_BLOCK_SIZE / 512 * 4) as u64);

    // Preallocating over the written prefix and the hole tail allocates only
    // the hole part.
    fx.file
        .fallocate(FallocateFlags::KEEP_SIZE, 0, (TEST_BLOCK_SIZE * 8) as u64)
        .unwrap();

    assert_eq!(
        inode.blocks(),
        blocks_before + (TEST_BLOCK_SIZE / 512 * 4) as u64
    );
    assert!(matches!(
        fx.ext4().read().map_blocks(&inode, LogicalBlock::new(0)),
        Ok(BlockMapping::Mapped { len, .. }) if len == BlockCount::new(4)
    ));
    assert!(matches!(
        fx.ext4().read().map_blocks(&inode, LogicalBlock::new(4)),
        Ok(BlockMapping::Unwritten { len, flags, .. })
            if len == BlockCount::new(4) && flags.is_empty()
    ));
    let mut output = vec![0u8; TEST_BLOCK_SIZE * 4];
    assert_eq!(
        fx.ext4().read().read_at(&inode, 0, &mut output).unwrap(),
        input.len()
    );
    assert_eq!(output, input);
}

/// Preallocation coexists with a delalloc reservation, and the later writeback
/// converts the unwritten extent in place.
#[def_test]
fn fallocate_reservation_coexists_with_delayed_allocation() {
    let fx = fallocate_vfs(TEST_FREE_BLOCKS, 0b0011_1111, 764);
    let inode = fx.inode_arc();
    let input = vec![0x33; TEST_BLOCK_SIZE * 4];
    let len = input.len() as u64;
    inode.set_size(len);
    fx.ext4()
        .write(fx.super_block.as_ref())
        .reserve_delalloc_range(&inode, LogicalBlock::new(0), 4)
        .unwrap();
    assert_eq!(fx.ext4().read().delalloc_reserved_block_count(), 4);

    // fallocate over the reserved range installs unwritten extents without
    // disturbing the reservation.
    fx.file.fallocate(FallocateFlags::KEEP_SIZE, 0, len).unwrap();
    assert!(matches!(
        fx.ext4().read().map_blocks(&inode, LogicalBlock::new(0)),
        Ok(BlockMapping::Unwritten { len, .. }) if len == BlockCount::new(4)
    ));

    // Delayed writeback lands on the preallocated blocks and converts them.
    fx.ext4()
        .write(fx.super_block.as_ref())
        .writeback_ordered_at(&inode, 0, &input, len, Ext4SyncIntent::FullMetadata, || {
            true
        })
        .unwrap();
    let mut output = vec![0u8; input.len()];
    assert_eq!(
        fx.ext4().read().read_at(&inode, 0, &mut output).unwrap(),
        input.len()
    );
    assert_eq!(output, input);
    assert_eq!(fx.ext4().read().delalloc_reserved_block_count(), 0);
    assert!(matches!(
        fx.ext4().read().map_blocks(&inode, LogicalBlock::new(0)),
        Ok(BlockMapping::Mapped { len, flags, .. })
            if len == BlockCount::new(4) && flags.is_empty()
    ));
}

/// ENOSPC keeps earlier committed batches as unwritten preallocation and the
/// VFS exit publishes the successfully allocated prefix as the visible size.
#[def_test]
fn fallocate_keeps_committed_batches_on_enospc() {
    let fx = fallocate_vfs(TEST_FREE_BLOCKS, 0b0011_1111, 765);

    // Mode 0 over far more blocks than the fixture's free space: an early batch
    // commits, a later one hits ENOSPC.
    let huge = (TEST_BLOCK_SIZE * 128) as u64;
    let result = fx.file.fallocate(FallocateFlags::empty(), 0, huge);

    assert!(result.is_err());
    // Earlier committed batches stay allocated as unwritten preallocation, and
    // the visible size is published to that prefix so writeback's
    // `visible_size < disk_size` guard stays satisfiable.
    assert!(matches!(
        fx.ext4().read().map_blocks(fx.inode(), LogicalBlock::new(0)),
        Ok(BlockMapping::Unwritten { flags, .. }) if flags.is_empty()
    ));
    assert_eq!(fx.free_blocks(), 0);
    let committed = fx.inode().disk_size();
    assert!(committed > 0);
    assert_eq!(fx.file.size(), committed);
}

/// A first-batch ENOSPC beyond EOF with !KEEP_SIZE must not advance
/// `i_disksize`: nothing was committed, matching Linux's `if (!epos) return ret`.
#[def_test]
fn fallocate_first_batch_enospc_beyond_eof_keeps_size() {
    let fx = fallocate_vfs(0, 0xff, 767);

    // Offset lands at logical block 2, past the empty file's EOF; with no free
    // blocks the first hole batch fails immediately, so nothing is committed.
    let offset = (TEST_BLOCK_SIZE * 2) as u64;
    let result = fx.file.fallocate(FallocateFlags::empty(), offset, TEST_BLOCK_SIZE as u64);

    assert!(result.is_err());
    assert_eq!(fx.file.size(), 0);
    assert_eq!(fx.inode().size(), 0);
    assert_eq!(fx.inode().disk_size(), 0);
}

/// Regression test for the fallocate/writeback size-publication race.
///
/// The device I/O hold parks the extension inside the ext4 core before it
/// advances the disk size while a writeback batch samples the visible size,
/// reproducing that interleaving instead of relying on scheduler timing.
#[def_test]
fn concurrent_fsync_and_fallocate_extension_both_succeed() {
    // A 512-byte setup write sets the committed baseline; each round extends
    // past its buffered write end, so every fallocate advances the disk size
    // and publishes a larger visible size. Three dirty blocks stay below the
    // speculative-preallocation threshold, fitting the fixture's budget.
    const TAIL_MARGIN: u64 = 512;
    const ROUND_WRITE_LEN: u64 = (TEST_BLOCK_SIZE * 3) as u64;
    const ROUNDS: usize = 4;

    let (mut filesystem, device) = journal_allocator_test_filesystem(160, 0b0011_1111);
    let private_inode = allocate_checkpointed_regular_inode(&mut filesystem);
    install_test_internal_journal(&mut filesystem, 766);
    let block_size = u64::from(filesystem.layout().block_size());
    let max_file_size = filesystem.extent_max_file_size().unwrap();
    let super_block = SuperBlock::new_with_flags_and_private(
        &FILE_SYSTEM_TYPE,
        &EXT4_SUPER_OPERATIONS,
        kalloc_types::KBox::new(Ext4Mount::new(filesystem)).unwrap(),
        SuperBlockFlags::empty(),
        block_size,
        max_file_size,
        |super_block| {
            let inode = Ext4SuperOperations::iget_from_private(super_block, private_inode).unwrap();
            Dentry::new_file_from_inode(inode, None, String::new())
        },
    );
    let mount = Mount::new_root(&super_block);
    let file = dentry_open(
        mount.root_path(),
        OpenFlags::READ_WRITE.bits(),
        kcred::initial_cred(),
    )
    .unwrap();

    let setup_data = vec![0x3e_u8; TAIL_MARGIN as usize];
    let mut pos = 0;
    assert_eq!(
        file.write_from(&setup_data, &mut pos),
        Ok(TAIL_MARGIN as usize)
    );
    file.fsync(false).unwrap();
    let mut committed_end = TAIL_MARGIN;

    for round in 0..ROUNDS {
        let write_end = committed_end + ROUND_WRITE_LEN;
        let extend_to = write_end + TAIL_MARGIN;
        let data = vec![0x40 + round as u8; ROUND_WRITE_LEN as usize];
        let mut pos = committed_end;
        assert_eq!(
            file.write_from(&data, &mut pos),
            Ok(ROUND_WRITE_LEN as usize)
        );

        // Freezing device I/O parks the extension inside the ext4 core (and
        // holds the invalidate lock) while writeback samples the visible size;
        // polling the parked-operation counter keeps the extension first.
        device.set_holds_io(true);
        let extension_file = file.clone();
        let extension_result: Arc<Mutex<VfsResult<()>>> = Arc::new(Mutex::new(Ok(())));
        let extension_slot = extension_result.clone();
        let extension_task = ktask::spawn(move || {
            *extension_slot.lock() =
                extension_file.fallocate(FallocateFlags::empty(), 0, extend_to);
        });
        for _ in 0..100 {
            if device.held_io_ops() != 0 {
                break;
            }
            ktask::sleep(ktime_types::TimeSpan::from_millis(1));
        }
        // Fail loudly if the parking premise breaks: without the park this
        // degrades into a scheduler-timing test. The size assert pins the park
        // to before the extension publishes; the buffered write published
        // `write_end`.
        assert!(
            device.held_io_ops() != 0,
            "extension did not park on device I/O inside the ext4 core"
        );
        assert_eq!(file.size(), write_end);
        let fsync_file = file.clone();
        let fsync_result: Arc<Mutex<VfsResult<()>>> = Arc::new(Mutex::new(Ok(())));
        let fsync_slot = fsync_result.clone();
        let fsync_task = ktask::spawn(move || {
            *fsync_slot.lock() = fsync_file.fsync(false);
        });
        ktask::sleep(ktime_types::TimeSpan::from_millis(50));
        device.set_holds_io(false);

        assert_eq!(extension_task.join(), 0);
        assert_eq!(fsync_task.join(), 0);
        assert_eq!(*extension_result.lock(), Ok(()));
        assert_eq!(*fsync_result.lock(), Ok(()));
        assert_eq!(file.size(), extend_to);
        committed_end = extend_to;
    }

    let ext4_inode = file.inode().private::<Ext4Inode>().unwrap();
    assert_eq!(ext4_inode.size(), committed_end);
    assert_eq!(ext4_inode.disk_size(), committed_end);

    let verify = |offset: u64, pattern: u8, len: u64| {
        let mut read_back = vec![0u8; len as usize];
        let mut pos = offset;
        assert_eq!(file.read_from(&mut read_back, &mut pos), Ok(len as usize));
        assert_eq!(read_back, vec![pattern; len as usize]);
    };
    verify(0, 0x3e, TAIL_MARGIN);
    let mut offset = TAIL_MARGIN;
    for round in 0..ROUNDS {
        verify(offset, 0x40 + round as u8, ROUND_WRITE_LEN);
        offset += ROUND_WRITE_LEN + TAIL_MARGIN;
    }
}
