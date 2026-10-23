// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! In-memory filesystem fixtures shared by kernel component tests.

use block::{Device, DeviceKind, DriverError, DriverResult};

use super::*;
use crate::{
    BlockCount, BlockGroupNumber, Ext4Inode, FilesystemBlock, LogicalBlock, PhysicalBlock,
    extent::ExtentMappingState,
    inode::InodeInitialization,
    jbd2::{JournalCredits, JournalTransactions, TransactionId},
    mballoc::{Ext4AllocationFlags, Ext4AllocationRequest},
    prealloc_space::{GroupPreallocSpace, PreallocSpace},
};

pub(crate) fn journal_allocator_test_filesystem(
    free_blocks: u32,
    block_bitmap_first_byte: u8,
) -> (Ext4SbInfo, Arc<TestDevice>) {
    let mut image = vec![0; TEST_BLOCK_SIZE * TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT];
    let mut superblock_bytes = allocator_superblock_with_geometry(
        TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT as u32,
        32,
        free_blocks,
        TEST_FREE_INODES,
    );
    put_u32(
        &mut superblock_bytes,
        0x20,
        TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT as u32,
    );
    put_u32(
        &mut superblock_bytes,
        0x24,
        TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT as u32,
    );
    image[1024..1024 + superblock::SUPERBLOCK_SIZE].copy_from_slice(&superblock_bytes);

    let descriptor = allocator_group_descriptor(free_blocks, TEST_FREE_INODES, 0, 0);
    image[TEST_BLOCK_SIZE..TEST_BLOCK_SIZE + descriptor.len()].copy_from_slice(&descriptor);

    let block_bitmap = &mut image[2 * TEST_BLOCK_SIZE..3 * TEST_BLOCK_SIZE];
    block_bitmap.fill(0xff);
    block_bitmap[0] = block_bitmap_first_byte;
    let mut remaining = free_blocks.saturating_sub(block_bitmap_first_byte.count_zeros());
    for block in 8..TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT {
        if (16..1040).contains(&block) || remaining == 0 {
            continue;
        }
        block_bitmap[block / 8] &= !(1 << (block % 8));
        remaining -= 1;
    }
    assert_eq!(remaining, 0);

    write_allocator_bitmap(&mut image, 3 * TEST_BLOCK_SIZE, &[0xff, 0x03, 0, 0], true);

    let block_device = Arc::new(TestDevice::new(image));
    let device: Arc<dyn BlockDeviceOperations> = block_device.clone();
    let filesystem_device = Arc::new(
        FilesystemDevice::open(
            device,
            TEST_BLOCK_SIZE,
            TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT as u64,
        )
        .unwrap(),
    );
    let metadata_io = Ext4MetadataIo::new(filesystem_device.clone());
    let superblock = Ext4DiskSuperblock::decode(&superblock_bytes).unwrap();
    let layout = FilesystemLayout::derive(&superblock).unwrap();
    let descriptors = [BlockGroupDescriptor::decode(&descriptor, true).unwrap()];
    let group_geometry = descriptors
        .iter()
        .map(GroupGeometry::from_descriptor)
        .collect();
    let groups: Vec<GroupMutableState> = descriptors
        .iter()
        .map(GroupMutableState::from_descriptor)
        .collect();
    let used_directories_count = groups.iter().fold(0u32, |total, state| {
        total.saturating_add(state.used_directories_count())
    });

    let mut filesystem = Ext4SbInfo {
        allocator: Mutex::new(AllocatorState {
            free_blocks_count: superblock.on_disk_free_blocks_count(),
            free_inodes_count: superblock.on_disk_free_inodes_count(),
            used_directories_count,
            last_orphan: superblock.last_orphan(),
            needs_recovery: superblock.features().needs_recovery(),
            has_orphan_present: superblock.features().has_orphan_present(),
            block_free_extent_caches: vec![None; groups.len()],
            preallocs_by_group: BTreeMap::new(),
            preallocated_blocks: 0,
            groups,
        }),
        delalloc_reserved_blocks: Mutex::new(0),
        superblock,
        group_geometry,
        statfs_mode: Ext4StatFsMode::Bsd,
        bitmap_maxbytes: 0,
        hash_unsigned: 0,
        device: filesystem_device,
        metadata_io,
        journal: None,
        orphan_info: None,
        layout,
        system_zones: Vec::new(),
    };
    filesystem.bitmap_maxbytes = filesystem.legacy_max_file_size().unwrap();
    let zones = filesystem.build_system_zones().unwrap();
    filesystem.system_zones = zones;
    (filesystem, block_device)
}

pub(crate) fn install_test_journal(
    filesystem: &mut Ext4SbInfo,
    journal_superblock: JournalSuperblock,
) {
    let journal_blocks = journal_superblock.max_blocks();
    let journal_block = FilesystemBlock::new(16);
    assert!(journal_block.get() + u64::from(journal_blocks) <= filesystem.layout().block_count);
    if !filesystem.is_system_zone_block(journal_block) {
        let mut zones = filesystem.system_zones.clone();
        add_system_zone_to(
            &mut zones,
            journal_block.get(),
            u64::from(journal_blocks),
            None,
            filesystem.layout().block_count,
        )
        .unwrap();
        filesystem.system_zones = zones;
    }
    let (block, offset, len) = filesystem.primary_superblock_location().unwrap();
    let end = offset + len;
    let mut bytes = vec![0; filesystem.device.block_size()];
    filesystem.device.read_blocks(block, 1, &mut bytes).unwrap();
    let superblock_bytes = bytes.get_mut(offset..end).unwrap();
    let compat = le_u32(superblock_bytes, 0x5c) | features::CompatFeatures::HAS_JOURNAL.bits();
    put_u32(superblock_bytes, 0x5c, compat);
    filesystem.superblock = Ext4DiskSuperblock::decode(superblock_bytes).unwrap();
    lock(&filesystem.allocator).has_orphan_present =
        filesystem.superblock.features().has_orphan_present();
    filesystem
        .device
        .write_contiguous_blocks(block, 1, &bytes)
        .unwrap();
    let journal_superblock_bytes = journal_superblock.encoded();
    let mut journal_block_bytes = vec![0; TEST_BLOCK_SIZE];
    journal_block_bytes[..journal_superblock_bytes.len()].copy_from_slice(journal_superblock_bytes);
    filesystem
        .write_contiguous_blocks(journal_block, 1, &journal_block_bytes)
        .unwrap();
    filesystem.journal = Some(
        MountedJournal::new(
            InternalJournal {
                superblock: journal_superblock,
                extents: vec![JournalExtent {
                    logical_start: 0,
                    physical_start: journal_block.get(),
                    len: journal_blocks,
                }],
                block_count: journal_blocks,
            },
            filesystem.layout().block_count,
        )
        .unwrap(),
    );
}

pub(crate) fn test_journal_superblock_bytes_with_blocks(
    sequence: u32,
    journal_blocks: u32,
) -> [u8; 1024] {
    let mut bytes = [0; 1024];
    put_be_u32(&mut bytes, 0x00, 0xc03b_3998);
    put_be_u32(&mut bytes, 0x04, 3);
    put_be_u32(&mut bytes, 0x0c, TEST_BLOCK_SIZE as u32);
    put_be_u32(&mut bytes, 0x10, journal_blocks);
    put_be_u32(&mut bytes, 0x14, 1);
    put_be_u32(&mut bytes, 0x18, sequence);
    put_be_u32(&mut bytes, 0x1c, 0);
    put_be_u32(&mut bytes, 0x58, 1);
    bytes
}

pub(crate) fn allocator_superblock_with_geometry(
    block_count: u32,
    inodes_count: u32,
    free_blocks: u32,
    free_inodes: u32,
) -> [u8; superblock::SUPERBLOCK_SIZE] {
    let mut bytes = [0; superblock::SUPERBLOCK_SIZE];
    put_u32(&mut bytes, 0x00, inodes_count);
    put_u32(&mut bytes, 0x04, block_count);
    put_u32(&mut bytes, 0x0c, free_blocks);
    put_u32(&mut bytes, 0x10, free_inodes);
    put_u32(&mut bytes, 0x14, 0);
    put_u32(&mut bytes, 0x18, 2);
    put_u32(&mut bytes, 0x1c, 2);
    put_u32(&mut bytes, 0x20, TEST_BLOCK_COUNT as u32);
    put_u32(&mut bytes, 0x24, TEST_BLOCK_COUNT as u32);
    put_u32(&mut bytes, 0x28, 32);
    put_u16(&mut bytes, 0x38, 0xef53);
    put_u32(&mut bytes, 0x4c, 1);
    put_u32(&mut bytes, 0x54, 11);
    put_u16(&mut bytes, 0x58, 256);
    put_u32(
        &mut bytes,
        0x60,
        features::IncompatFeatures::EXTENTS
            .union(features::IncompatFeatures::BIT_64)
            .bits(),
    );
    put_u16(&mut bytes, 0xfe, 64);
    bytes
}

pub(crate) fn allocator_group_descriptor(
    free_blocks: u32,
    free_inodes: u32,
    used_directories: u32,
    flags: u16,
) -> [u8; 64] {
    let mut bytes = [0; 64];
    put_u32(&mut bytes, 0, 2);
    put_u32(&mut bytes, 4, 3);
    put_u32(&mut bytes, 8, 4);
    put_u16(&mut bytes, 12, free_blocks as u16);
    put_u16(&mut bytes, 14, free_inodes as u16);
    put_u16(&mut bytes, 16, used_directories as u16);
    put_u16(&mut bytes, 18, flags);
    bytes
}

pub(crate) fn write_allocator_bitmap(
    image: &mut [u8],
    offset: usize,
    prefix: &[u8],
    initialized: bool,
) {
    let bitmap = &mut image[offset..offset + TEST_BLOCK_SIZE];
    bitmap[..prefix.len()].copy_from_slice(prefix);
    if initialized {
        bitmap[prefix.len()..].fill(0xff);
    }
}

pub(crate) fn le_u32(bytes: &[u8], offset: usize) -> u32 {
    u32::from_le_bytes(bytes[offset..offset + 4].try_into().unwrap())
}

pub(crate) fn put_u16(output: &mut [u8], offset: usize, value: u16) {
    output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

pub(crate) fn put_be_u32(output: &mut [u8], offset: usize, value: u32) {
    output[offset..offset + 4].copy_from_slice(&value.to_be_bytes());
}

pub(crate) struct TestDevice {
    pub(crate) bytes: Mutex<Vec<u8>>,
    pub(crate) flush_count: Mutex<usize>,
    pub(crate) fail_flush_at: Mutex<Option<usize>>,
    write_count: Mutex<usize>,
    fail_write_at: Mutex<Option<usize>>,
}

impl TestDevice {
    pub(crate) fn new(bytes: Vec<u8>) -> Self {
        Self {
            bytes: Mutex::new(bytes),
            flush_count: Mutex::new(0),
            fail_flush_at: Mutex::new(None),
            write_count: Mutex::new(0),
            fail_write_at: Mutex::new(None),
        }
    }

    pub(crate) fn bytes(&self) -> Vec<u8> {
        lock(&self.bytes).clone()
    }

    pub(crate) fn write_count(&self) -> usize {
        *lock(&self.write_count)
    }

    pub(crate) fn fail_write_at(&self, write_count: usize) {
        *lock(&self.fail_write_at) = Some(write_count);
    }

    pub(crate) fn flush_count(&self) -> usize {
        *lock(&self.flush_count)
    }

    pub(crate) fn fail_flush_at(&self, flush_count: usize) {
        *lock(&self.fail_flush_at) = Some(flush_count);
    }
}

impl Device for TestDevice {
    fn name(&self) -> &str {
        "kext4-mount-allocator-test"
    }

    fn device_kind(&self) -> DeviceKind {
        DeviceKind::Block
    }
}

impl BlockDeviceOperations for TestDevice {
    fn num_blocks(&self) -> u64 {
        (lock(&self.bytes).len() / TEST_BLOCK_SIZE) as u64
    }

    fn block_size(&self) -> usize {
        TEST_BLOCK_SIZE
    }

    fn read_block(&self, block_id: u64, output: &mut [u8]) -> DriverResult {
        let start = usize::try_from(block_id)
            .ok()
            .and_then(|block| block.checked_mul(TEST_BLOCK_SIZE))
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(output.len())
            .ok_or(DriverError::InvalidInput)?;
        output.copy_from_slice(
            lock(&self.bytes)
                .get(start..end)
                .ok_or(DriverError::InvalidInput)?,
        );
        Ok(())
    }

    fn write_block(&self, block_id: u64, input: &[u8]) -> DriverResult {
        let mut write_count = lock(&self.write_count);
        *write_count += 1;
        if *lock(&self.fail_write_at) == Some(*write_count) {
            return Err(DriverError::Io);
        }
        drop(write_count);
        let start = usize::try_from(block_id)
            .ok()
            .and_then(|block| block.checked_mul(TEST_BLOCK_SIZE))
            .ok_or(DriverError::InvalidInput)?;
        let end = start
            .checked_add(input.len())
            .ok_or(DriverError::InvalidInput)?;
        lock(&self.bytes)
            .get_mut(start..end)
            .ok_or(DriverError::InvalidInput)?
            .copy_from_slice(input);
        Ok(())
    }

    fn flush(&self) -> DriverResult {
        let mut flush_count = lock(&self.flush_count);
        *flush_count += 1;
        if *lock(&self.fail_flush_at) == Some(*flush_count) {
            return Err(DriverError::Io);
        }
        Ok(())
    }
}

pub(crate) const TEST_BLOCK_SIZE: usize = 4096;

pub(crate) const TEST_BLOCK_COUNT: usize = 32;

pub(crate) const TEST_JOURNAL_FILESYSTEM_BLOCK_COUNT: usize = 2048;

pub(crate) const TEST_FREE_BLOCKS: u32 = 26;

pub(crate) const TEST_FREE_INODES: u32 = 22;

/// Minimal reopenable metadata for recovery tests: one group, root and journal.
/// This is not an e2fsprogs interoperability image; unneeded inodes stay zero.
pub(crate) fn recovery_test_filesystem() -> (Ext4SbInfo, Arc<TestDevice>) {
    let (filesystem, device) = journal_allocator_test_filesystem(TEST_FREE_BLOCKS - 1, 0x7f);
    drop(filesystem);
    {
        let mut image = lock(&device.bytes);
        let sb = &mut image[1024..2048];
        put_u32(sb, 0x5c, features::CompatFeatures::HAS_JOURNAL.bits());
        let incompat = le_u32(sb, 0x60) | features::IncompatFeatures::FILETYPE.bits();
        put_u32(sb, 0x60, incompat);
        put_u32(sb, 0xe0, 8); // Reserved internal journal inode.
        put_u16(&mut image[TEST_BLOCK_SIZE..], 16, 1);
        for (number, mode, links, start, blocks) in
            [(2usize, 0x41ed, 2, 6u32, 1u16), (8, 0x8180, 1, 16, 1024)]
        {
            let offset = 4 * TEST_BLOCK_SIZE + (number - 1) * 256;
            let inode = &mut image[offset..offset + 256];
            put_u16(inode, 0, mode);
            put_u32(inode, 4, u32::from(blocks) * TEST_BLOCK_SIZE as u32);
            put_u16(inode, 0x1a, links);
            put_u32(inode, 0x1c, u32::from(blocks) * 8);
            put_u32(inode, 0x20, crate::disk::inode::EXT4_EXTENTS_FL);
            put_u16(inode, 0x28, 0xf30a);
            put_u16(inode, 0x2a, 1);
            put_u16(inode, 0x2c, 4);
            put_u16(inode, 0x38, blocks);
            put_u32(inode, 0x3c, start);
        }
        let directory = &mut image[6 * TEST_BLOCK_SIZE..7 * TEST_BLOCK_SIZE];
        for (offset, len, name) in [
            (0, 12, b".".as_slice()),
            (12, TEST_BLOCK_SIZE - 12, b"..".as_slice()),
        ] {
            put_u32(directory, offset, 2);
            put_u16(directory, offset + 4, len as u16);
            directory[offset + 6] = name.len() as u8;
            directory[offset + 7] = 2;
            directory[offset + 8..offset + 8 + name.len()].copy_from_slice(name);
        }
        let mut journal = test_journal_superblock_bytes_with_blocks(500, 1024);
        put_be_u32(&mut journal, 4, 4);
        crate::jbd2::enable_superblock_revoke(&mut journal).unwrap();
        image[16 * TEST_BLOCK_SIZE..16 * TEST_BLOCK_SIZE + journal.len()].copy_from_slice(&journal);
    }
    (open_test_filesystem(device.clone()).unwrap(), device)
}

pub(crate) fn open_test_filesystem(
    device: Arc<dyn BlockDeviceOperations>,
) -> Ext4Result<Ext4SbInfo> {
    let mut filesystem = Ext4SbInfo::prepare_mount_with_statfs_mode(device, Ext4StatFsMode::Bsd)?;
    filesystem.persist_directory_hash_policy()?;
    Ok(filesystem)
}

pub(crate) fn update_test_inode_links_count(
    filesystem: &mut Ext4SbInfo,
    inode: &crate::Ext4Inode,
    links_count: u16,
    handle: &mut crate::jbd2::JournalHandle<'_>,
) {
    let inode_table_block = filesystem.inode_table_entry_block(inode.number()).unwrap();
    let inode_table_access = filesystem
        .metadata_io
        .write_access(inode_table_block, handle)
        .unwrap();
    let mut inode_table_bytes = metadata_access_bytes(&inode_table_access).unwrap();
    let updated = filesystem
        .update_inode_table_entry_allow_zero_links(
            &mut inode_table_bytes,
            inode.number(),
            |inode_bytes| {
                put_u16(inode_bytes, 0x1a, links_count);
                Ok(())
            },
        )
        .unwrap();
    replace_metadata_access_bytes(&inode_table_access, inode_table_bytes).unwrap();
    filesystem.publish_inode_metadata(inode, updated).unwrap();
}

pub(crate) fn set_allocator_feature_bits(
    filesystem: &mut Ext4SbInfo,
    compat_features: features::CompatFeatures,
    read_only_compat_features: features::ReadOnlyCompatFeatures,
) {
    let (block, offset, len) = filesystem.primary_superblock_location().unwrap();
    let end = offset + len;
    let mut bytes = vec![0; filesystem.device.block_size()];
    filesystem.device.read_blocks(block, 1, &mut bytes).unwrap();
    let superblock_bytes = bytes.get_mut(offset..end).unwrap();
    let compat = le_u32(superblock_bytes, 0x5c) | compat_features.bits();
    let ro_compat = le_u32(superblock_bytes, 0x64) | read_only_compat_features.bits();
    put_u32(superblock_bytes, 0x5c, compat);
    put_u32(superblock_bytes, 0x64, ro_compat);
    if features::ReadOnlyCompatFeatures::from_bits_retain(ro_compat)
        .contains(features::ReadOnlyCompatFeatures::METADATA_CSUM)
    {
        update_allocator_superblock_checksum(superblock_bytes);
    }
    filesystem.superblock = Ext4DiskSuperblock::decode(superblock_bytes).unwrap();
    lock(&filesystem.allocator).has_orphan_present =
        filesystem.superblock.features().has_orphan_present();
    filesystem
        .device
        .write_contiguous_blocks(block, 1, &bytes)
        .unwrap();
}

#[derive(Clone, Copy)]
pub(crate) struct AllocatorGroupSpec {
    pub(crate) free_blocks: u32,
    pub(crate) free_inodes: u32,
    pub(crate) used_directories: u32,
    pub(crate) flags: u16,
    pub(crate) block_bitmap: [u8; 4],
    pub(crate) inode_bitmap: [u8; 4],
}

pub(crate) fn allocator_multigroup_test_filesystem(
    groups: &[AllocatorGroupSpec],
) -> (Ext4SbInfo, Arc<TestDevice>) {
    allocator_multigroup_test_filesystem_inner(groups, false, &[])
}

/// Multi-group fixture with metadata checksums enabled. Each group index in
/// `corrupt_block_bitmap_groups` gets a block bitmap checksum that does not
/// match its bitmap, while the group descriptor itself keeps a valid
/// checksum: bitmap reads fail with `InvalidBlockBitmap` and the scan can
/// still reach the remaining healthy groups.
pub(crate) fn allocator_multigroup_checksum_test_filesystem(
    groups: &[AllocatorGroupSpec],
    corrupt_block_bitmap_groups: &[u32],
) -> (Ext4SbInfo, Arc<TestDevice>) {
    allocator_multigroup_test_filesystem_inner(groups, true, corrupt_block_bitmap_groups)
}

fn allocator_multigroup_test_filesystem_inner(
    groups: &[AllocatorGroupSpec],
    has_metadata_checksum: bool,
    corrupt_block_bitmap_groups: &[u32],
) -> (Ext4SbInfo, Arc<TestDevice>) {
    let group_count = u32::try_from(groups.len()).unwrap();
    let block_count = group_count * TEST_BLOCK_COUNT as u32;
    let inodes_count = group_count * 32;
    let free_blocks = groups
        .iter()
        .map(|group| group.free_blocks)
        .try_fold(0u32, |sum, value| sum.checked_add(value))
        .unwrap();
    let free_inodes = groups
        .iter()
        .map(|group| group.free_inodes)
        .try_fold(0u32, |sum, value| sum.checked_add(value))
        .unwrap();
    let mut image = vec![0; usize::try_from(block_count).unwrap() * TEST_BLOCK_SIZE];
    let mut superblock_bytes =
        allocator_superblock_with_geometry(block_count, inodes_count, free_blocks, free_inodes);
    if has_metadata_checksum {
        enable_allocator_metadata_checksum(&mut superblock_bytes);
    }
    image[1024..1024 + superblock::SUPERBLOCK_SIZE].copy_from_slice(&superblock_bytes);
    let superblock = Ext4DiskSuperblock::decode(&superblock_bytes).unwrap();

    let mut descriptors = Vec::new();
    for (index, group) in groups.iter().copied().enumerate() {
        let group_number = u32::try_from(index).unwrap();
        let group_first = index * TEST_BLOCK_COUNT;
        let block_bitmap_start = (group_first + 2) * TEST_BLOCK_SIZE;
        write_allocator_bitmap(
            &mut image,
            block_bitmap_start,
            &group.block_bitmap,
            group.flags & 0x0002 == 0,
        );
        let inode_bitmap_start = (group_first + 3) * TEST_BLOCK_SIZE;
        write_allocator_bitmap(
            &mut image,
            inode_bitmap_start,
            &group.inode_bitmap,
            group.flags & 0x0001 == 0,
        );

        let mut descriptor = allocator_group_descriptor_for_group(group_number, group);
        if has_metadata_checksum {
            update_allocator_descriptor_bitmap_checksums(
                &mut descriptor,
                group_number,
                superblock.checksum_seed(),
                &image[block_bitmap_start..block_bitmap_start + TEST_BLOCK_SIZE],
                &image[inode_bitmap_start..inode_bitmap_start + TEST_BLOCK_SIZE],
                corrupt_block_bitmap_groups.contains(&group_number),
                false,
            );
        }
        descriptors.push(descriptor);
        let descriptor_start = TEST_BLOCK_SIZE + index * 64;
        image[descriptor_start..descriptor_start + 64].copy_from_slice(&descriptor);
    }

    let block_device = Arc::new(TestDevice::new(image));
    let device: Arc<dyn BlockDeviceOperations> = block_device.clone();
    let filesystem_device =
        Arc::new(FilesystemDevice::open(device, TEST_BLOCK_SIZE, u64::from(block_count)).unwrap());
    let metadata_io = Ext4MetadataIo::new(filesystem_device.clone());
    let layout = FilesystemLayout::derive(&superblock).unwrap();
    let descriptors: Vec<BlockGroupDescriptor> = descriptors
        .iter()
        .map(|descriptor| BlockGroupDescriptor::decode(descriptor, true).unwrap())
        .collect();
    let group_geometry = descriptors
        .iter()
        .map(GroupGeometry::from_descriptor)
        .collect();
    let groups: Vec<GroupMutableState> = descriptors
        .iter()
        .map(GroupMutableState::from_descriptor)
        .collect();
    let used_directories_count = groups.iter().fold(0u32, |total, state| {
        total.saturating_add(state.used_directories_count())
    });

    let mut filesystem = Ext4SbInfo {
        allocator: Mutex::new(AllocatorState {
            free_blocks_count: superblock.on_disk_free_blocks_count(),
            free_inodes_count: superblock.on_disk_free_inodes_count(),
            used_directories_count,
            last_orphan: superblock.last_orphan(),
            needs_recovery: superblock.features().needs_recovery(),
            has_orphan_present: superblock.features().has_orphan_present(),
            block_free_extent_caches: vec![None; groups.len()],
            preallocs_by_group: BTreeMap::new(),
            preallocated_blocks: 0,
            groups,
        }),
        delalloc_reserved_blocks: Mutex::new(0),
        superblock,
        group_geometry,
        statfs_mode: Ext4StatFsMode::Bsd,
        bitmap_maxbytes: 0,
        hash_unsigned: 0,
        device: filesystem_device,
        metadata_io,
        journal: None,
        orphan_info: None,
        layout,
        system_zones: Vec::new(),
    };
    filesystem.bitmap_maxbytes = filesystem.legacy_max_file_size().unwrap();
    let zones = filesystem.build_system_zones().unwrap();
    filesystem.system_zones = zones;
    (filesystem, block_device)
}

pub(crate) fn allocator_group_descriptor_for_group(
    group: u32,
    spec: AllocatorGroupSpec,
) -> [u8; 64] {
    let group_first = group * TEST_BLOCK_COUNT as u32;
    let mut bytes = [0; 64];
    put_u32(&mut bytes, 0, group_first + 2);
    put_u32(&mut bytes, 4, group_first + 3);
    put_u32(&mut bytes, 8, group_first + 4);
    put_u16(&mut bytes, 12, spec.free_blocks as u16);
    put_u16(&mut bytes, 14, spec.free_inodes as u16);
    put_u16(&mut bytes, 16, spec.used_directories as u16);
    put_u16(&mut bytes, 18, spec.flags);
    bytes
}

pub(crate) fn fragment_ordered_writeback_free_space(filesystem: &mut Ext4SbInfo) {
    let journal = JournalTransactions::new(TransactionId::new(713));
    let mut handle = journal.begin(JournalCredits::new(64)).unwrap();
    let transaction = handle.id();
    for physical in [
        7u64, 9, 11, 13, 15, 1041, 1043, 1045, 1047, 1049, 1051, 1053, 1054, 1055,
    ] {
        let request = Ext4AllocationRequest::for_metadata(
            Some(FilesystemBlock::new(physical)),
            BlockCount::new(1),
            BlockCount::new(1),
            Ext4AllocationFlags::EXACT,
        )
        .unwrap();
        let allocation = filesystem
            .allocate_blocks_for_write(request, &mut handle)
            .unwrap();
        assert_eq!(allocation.physical_start(), PhysicalBlock::new(physical));
    }
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
    assert_eq!(filesystem.free_blocks_count(), 12);
}

pub(crate) fn allocator_test_filesystem(
    free_blocks: u32,
    bitmap_first_byte: u8,
) -> (Ext4SbInfo, Arc<TestDevice>) {
    allocator_test_filesystem_with_inodes(free_blocks, bitmap_first_byte, TEST_FREE_INODES, 0x03, 0)
}

pub(crate) fn allocator_test_filesystem_with_inodes(
    free_blocks: u32,
    block_bitmap_first_byte: u8,
    free_inodes: u32,
    inode_bitmap_second_byte: u8,
    used_directories: u32,
) -> (Ext4SbInfo, Arc<TestDevice>) {
    allocator_test_filesystem_with_flags(
        free_blocks,
        block_bitmap_first_byte,
        free_inodes,
        inode_bitmap_second_byte,
        used_directories,
        0,
    )
}

pub(crate) fn allocator_test_filesystem_with_flags(
    free_blocks: u32,
    block_bitmap_first_byte: u8,
    free_inodes: u32,
    inode_bitmap_second_byte: u8,
    used_directories: u32,
    flags: u16,
) -> (Ext4SbInfo, Arc<TestDevice>) {
    allocator_test_filesystem_with_options(
        AllocatorGroupSpec {
            free_blocks,
            free_inodes,
            used_directories,
            flags,
            block_bitmap: [block_bitmap_first_byte, 0, 0, 0],
            inode_bitmap: [0xff, inode_bitmap_second_byte, 0, 0],
        },
        false,
        false,
        false,
    )
}

pub(crate) fn allocator_test_filesystem_with_options(
    spec: AllocatorGroupSpec,
    has_metadata_checksum: bool,
    corrupt_block_bitmap_checksum: bool,
    corrupt_inode_bitmap_checksum: bool,
) -> (Ext4SbInfo, Arc<TestDevice>) {
    let mut image = vec![0; TEST_BLOCK_SIZE * TEST_BLOCK_COUNT];
    let mut superblock_bytes = allocator_superblock(spec.free_blocks, spec.free_inodes);
    if has_metadata_checksum {
        enable_allocator_metadata_checksum(&mut superblock_bytes);
    }
    image[1024..1024 + superblock::SUPERBLOCK_SIZE].copy_from_slice(&superblock_bytes);

    let mut descriptor = allocator_group_descriptor_for_group(0, spec);
    write_allocator_bitmap(
        &mut image,
        2 * TEST_BLOCK_SIZE,
        &spec.block_bitmap,
        spec.flags & TEST_EXT4_BG_BLOCK_UNINIT == 0,
    );
    write_allocator_bitmap(
        &mut image,
        3 * TEST_BLOCK_SIZE,
        &spec.inode_bitmap,
        spec.flags & TEST_EXT4_BG_INODE_UNINIT == 0,
    );

    let superblock = Ext4DiskSuperblock::decode(&superblock_bytes).unwrap();
    if has_metadata_checksum {
        update_allocator_descriptor_bitmap_checksums(
            &mut descriptor,
            0,
            superblock.checksum_seed(),
            &image[2 * TEST_BLOCK_SIZE..3 * TEST_BLOCK_SIZE],
            &image[3 * TEST_BLOCK_SIZE..4 * TEST_BLOCK_SIZE],
            corrupt_block_bitmap_checksum,
            corrupt_inode_bitmap_checksum,
        );
    }
    image[TEST_BLOCK_SIZE..TEST_BLOCK_SIZE + descriptor.len()].copy_from_slice(&descriptor);

    let block_device = Arc::new(TestDevice::new(image));
    let device: Arc<dyn BlockDeviceOperations> = block_device.clone();
    let filesystem_device =
        Arc::new(FilesystemDevice::open(device, TEST_BLOCK_SIZE, TEST_BLOCK_COUNT as u64).unwrap());
    let metadata_io = Ext4MetadataIo::new(filesystem_device.clone());
    let layout = FilesystemLayout::derive(&superblock).unwrap();
    let descriptors = [BlockGroupDescriptor::decode(&descriptor, true).unwrap()];
    let group_geometry = descriptors
        .iter()
        .map(GroupGeometry::from_descriptor)
        .collect();
    let groups: Vec<GroupMutableState> = descriptors
        .iter()
        .map(GroupMutableState::from_descriptor)
        .collect();
    let used_directories_count = groups.iter().fold(0u32, |total, state| {
        total.saturating_add(state.used_directories_count())
    });

    let mut filesystem = Ext4SbInfo {
        allocator: Mutex::new(AllocatorState {
            free_blocks_count: superblock.on_disk_free_blocks_count(),
            free_inodes_count: superblock.on_disk_free_inodes_count(),
            used_directories_count,
            last_orphan: superblock.last_orphan(),
            needs_recovery: superblock.features().needs_recovery(),
            has_orphan_present: superblock.features().has_orphan_present(),
            block_free_extent_caches: vec![None; groups.len()],
            preallocs_by_group: BTreeMap::new(),
            preallocated_blocks: 0,
            groups,
        }),
        delalloc_reserved_blocks: Mutex::new(0),
        superblock,
        group_geometry,
        statfs_mode: Ext4StatFsMode::Bsd,
        bitmap_maxbytes: 0,
        hash_unsigned: 0,
        device: filesystem_device,
        metadata_io,
        journal: None,
        orphan_info: None,
        layout,
        system_zones: Vec::new(),
    };
    filesystem.bitmap_maxbytes = filesystem.legacy_max_file_size().unwrap();
    let zones = filesystem.build_system_zones().unwrap();
    filesystem.system_zones = zones;
    (filesystem, block_device)
}

pub(crate) fn allocate_contiguous_blocks(
    filesystem: &mut Ext4SbInfo,
    count: u32,
    handle: &mut crate::jbd2::JournalHandle<'_>,
) -> PhysicalBlock {
    let mut first = None;
    for index in 0..count {
        let allocation = filesystem.allocate_block(None, handle).unwrap();
        if index == 0 {
            first = Some(allocation.block());
        } else {
            assert_eq!(
                allocation.block(),
                PhysicalBlock::new(first.unwrap().get() + u64::from(index))
            );
        }
    }
    first.unwrap()
}

pub(crate) fn allocator_superblock(
    free_blocks: u32,
    free_inodes: u32,
) -> [u8; superblock::SUPERBLOCK_SIZE] {
    allocator_superblock_with_geometry(TEST_BLOCK_COUNT as u32, 32, free_blocks, free_inodes)
}

pub(crate) fn allocate_checkpointed_regular_inode(filesystem: &mut Ext4SbInfo) -> crate::Ext4Inode {
    allocate_checkpointed_inode(filesystem, InodeInitialization::regular_file(0o644, 0, 0))
}

pub(crate) fn allocate_checkpointed_directory_inode(
    filesystem: &mut Ext4SbInfo,
) -> crate::Ext4Inode {
    allocate_checkpointed_inode(filesystem, InodeInitialization::directory(0o755, 0, 0))
}

pub(crate) fn allocate_checkpointed_inode(
    filesystem: &mut Ext4SbInfo,
    initialization: InodeInitialization,
) -> crate::Ext4Inode {
    let journal = JournalTransactions::new(TransactionId::new(690));
    let mut handle = journal.begin(JournalCredits::new(8)).unwrap();
    let transaction = handle.id();
    let allocation = filesystem
        .allocate_inode(None, initialization, &mut handle)
        .unwrap();
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
    filesystem.internal_iget(allocation.inode()).unwrap()
}

pub(crate) fn persist_test_orphan_head(
    filesystem: &mut Ext4SbInfo,
    head: Option<InodeNumber>,
    sequence: u32,
) {
    let journal = JournalTransactions::new(TransactionId::new(sequence));
    let mut handle = journal.begin(JournalCredits::new(4)).unwrap();
    let transaction = handle.id();
    filesystem.set_orphan_head(head, &mut handle).unwrap();
    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();
}

pub(crate) fn update_test_inode_extra_isize(
    filesystem: &mut Ext4SbInfo,
    inode: &Ext4Inode,
    extra_isize: u16,
    handle: &mut crate::jbd2::JournalHandle<'_>,
) {
    let inode_table_block = filesystem.inode_table_entry_block(inode.number()).unwrap();
    let inode_table_access = filesystem
        .metadata_io
        .write_access(inode_table_block, handle)
        .unwrap();
    let mut inode_table_bytes = metadata_access_bytes(&inode_table_access).unwrap();
    let updated = filesystem
        .update_referenced_inode_table_entry(&mut inode_table_bytes, inode, |inode_bytes| {
            put_u16(
                inode_bytes,
                crate::disk::inode::EXTRA_ISIZE_OFFSET,
                extra_isize,
            );
            Ok(())
        })
        .unwrap();
    replace_metadata_access_bytes(&inode_table_access, inode_table_bytes).unwrap();
    filesystem.publish_inode_metadata(inode, updated).unwrap();
}

pub(crate) fn set_allocator_default_hash_version(filesystem: &mut Ext4SbInfo, hash_version: u8) {
    let (block, offset, len) = filesystem.primary_superblock_location().unwrap();
    let end = offset + len;
    let mut bytes = vec![0; filesystem.device.block_size()];
    filesystem.device.read_blocks(block, 1, &mut bytes).unwrap();
    let superblock_bytes = bytes.get_mut(offset..end).unwrap();
    superblock_bytes[0xfc] = hash_version;
    filesystem.superblock = Ext4DiskSuperblock::decode(superblock_bytes).unwrap();
    lock(&filesystem.allocator).has_orphan_present =
        filesystem.superblock.features().has_orphan_present();
    filesystem
        .device
        .write_contiguous_blocks(block, 1, &bytes)
        .unwrap();
}

pub(crate) fn enable_allocator_flex_bg(filesystem: &mut Ext4SbInfo, log_groups_per_flex: u8) {
    let (block, offset, len) = filesystem.primary_superblock_location().unwrap();
    let end = offset + len;
    let mut bytes = vec![0; filesystem.device.block_size()];
    filesystem.device.read_blocks(block, 1, &mut bytes).unwrap();
    let superblock_bytes = bytes.get_mut(offset..end).unwrap();
    let incompat = le_u32(superblock_bytes, 0x60) | features::IncompatFeatures::FLEX_BG.bits();
    put_u32(superblock_bytes, 0x60, incompat);
    superblock_bytes[0x174] = log_groups_per_flex;
    let updated = Ext4DiskSuperblock::decode(superblock_bytes).unwrap();
    filesystem.layout = FilesystemLayout::derive(&updated).unwrap();
    filesystem.superblock = updated;
    filesystem
        .device
        .write_contiguous_blocks(block, 1, &bytes)
        .unwrap();
}

pub(crate) fn test_journal_superblock(sequence: u32) -> JournalSuperblock {
    let bytes = test_journal_superblock_bytes_with_blocks(sequence, 1024);
    JournalSuperblock::decode(&bytes, TEST_BLOCK_SIZE as u32, 1024, [0; 16]).unwrap()
}

pub(crate) fn enable_allocator_metadata_checksum(bytes: &mut [u8]) {
    put_u32(
        bytes,
        0x64,
        features::ReadOnlyCompatFeatures::METADATA_CSUM.bits(),
    );
    bytes[0x175] = 1;
    update_allocator_superblock_checksum(bytes);
}

pub(crate) fn update_allocator_superblock_checksum(bytes: &mut [u8]) {
    let checksum = checksum::crc32c(u32::MAX, &bytes[..0x3fc]);
    put_u32(bytes, 0x3fc, checksum);
}

pub(crate) fn update_allocator_descriptor_bitmap_checksums(
    descriptor: &mut [u8],
    group: u32,
    checksum_seed: u32,
    block_bitmap: &[u8],
    inode_bitmap: &[u8],
    corrupt_block_bitmap_checksum: bool,
    corrupt_inode_bitmap_checksum: bool,
) {
    let block_checksum = maybe_corrupt_checksum(
        checksum::bitmap_checksum(&block_bitmap[..TEST_BLOCK_COUNT / 8], checksum_seed),
        corrupt_block_bitmap_checksum,
    );
    let inode_checksum = maybe_corrupt_checksum(
        checksum::bitmap_checksum(&inode_bitmap[..TEST_BLOCK_COUNT / 8], checksum_seed),
        corrupt_inode_bitmap_checksum,
    );

    put_u16(descriptor, 24, block_checksum as u16);
    put_u16(descriptor, 26, inode_checksum as u16);
    put_u16(descriptor, 56, (block_checksum >> 16) as u16);
    put_u16(descriptor, 58, (inode_checksum >> 16) as u16);
    put_u16(descriptor, 30, 0);
    let descriptor_checksum =
        checksum::group_descriptor_checksum(descriptor, group, checksum_seed).unwrap();
    put_u16(descriptor, 30, descriptor_checksum);
}

pub(crate) fn maybe_corrupt_checksum(checksum: u32, corrupt: bool) -> u32 {
    if corrupt { checksum ^ 1 } else { checksum }
}

pub(crate) fn le_u16(bytes: &[u8], offset: usize) -> u16 {
    u16::from_le_bytes(bytes[offset..offset + 2].try_into().unwrap())
}

pub(crate) fn eviction_test_unlinked_inode(
    extents: &[(u64, u32)],
) -> (Ext4SbInfo, Ext4Inode, crate::Ext4Timestamp) {
    let (mut filesystem, _device) = journal_allocator_test_filesystem(512, 0b0011_1111);
    install_test_internal_journal(&mut filesystem, 2000);
    let journal = JournalTransactions::new(TransactionId::new(2000));
    let mut handle = journal.begin(JournalCredits::new(100_000)).unwrap();
    let transaction = handle.id();

    let inode_allocation = filesystem
        .allocate_inode(
            None,
            InodeInitialization::regular_file(0o644, 0, 0),
            &mut handle,
        )
        .unwrap();
    let inode = filesystem.internal_iget(inode_allocation.inode()).unwrap();

    let mut next_physical = FilesystemBlock::new(1040);
    for (logical, len) in extents {
        let allocation = filesystem
            .allocate_block_run_in_group(
                BlockGroupNumber::new(0),
                Some(next_physical),
                BlockCount::new(*len),
                BlockCount::new(*len),
                &mut handle,
            )
            .unwrap();
        assert_eq!(allocation.first_block().get(), next_physical.get());
        let physical = allocation.first_block();
        next_physical = FilesystemBlock::new(next_physical.get() + u64::from(*len));
        filesystem
            .insert_extent_mapping(
                &inode,
                LogicalBlock::new(*logical),
                physical,
                BlockCount::new(*len),
                ExtentMappingState::Initialized,
                &mut handle,
            )
            .unwrap();
    }

    let timestamp = crate::Ext4Timestamp::new(2000, 0);
    filesystem
        .update_unlinked_inode_metadata(&inode, None, timestamp, &mut handle)
        .unwrap();

    drop(handle);
    let commit = journal.force_commit(transaction).unwrap();
    filesystem
        .metadata_io
        .checkpoint_committed(&commit)
        .unwrap();
    journal.finish_checkpoint_for_test(&commit).unwrap();

    // Reset the internal journal so the eviction protocol begins from a
    // clean journal with a fresh sequence, matching the installed
    // superblock (mirrors the per-phase reinstall used elsewhere).
    install_test_internal_journal(&mut filesystem, 2001);

    (filesystem, inode, timestamp)
}

pub(crate) fn install_test_internal_journal(filesystem: &mut Ext4SbInfo, sequence: u32) {
    install_test_journal(filesystem, test_journal_superblock(sequence));
}

pub(crate) const TEST_EXT4_BG_INODE_UNINIT: u16 = 1;
pub(crate) const TEST_EXT4_BG_BLOCK_UNINIT: u16 = 2;

pub(crate) fn install_credit_limited_journal(
    filesystem: &mut Ext4SbInfo,
    sequence: u32,
    credits: u32,
    has_revoke: bool,
) {
    let mut bytes = test_journal_superblock_bytes_with_blocks(sequence, 1024);
    if has_revoke {
        put_be_u32(&mut bytes, 4, 4);
        crate::jbd2::enable_superblock_revoke(&mut bytes).unwrap();
    }
    let superblock =
        JournalSuperblock::decode(&bytes, TEST_BLOCK_SIZE as u32, 1024, [0; 16]).unwrap();
    install_test_journal(filesystem, superblock);
    crate::journal::tests::limit_transaction_credits(filesystem, credits);
}

pub(crate) fn orphan_test_filesystem() -> (Ext4SbInfo, Ext4Inode) {
    let (mut filesystem, _device) = allocator_test_filesystem(TEST_FREE_BLOCKS, 0b0011_1111);
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::ORPHAN_FILE,
        features::ReadOnlyCompatFeatures::empty(),
    );
    (filesystem, inode)
}

pub(crate) fn checksummed_orphan_test_filesystem() -> (Ext4SbInfo, Ext4Inode) {
    let (mut filesystem, _device) = allocator_test_filesystem_with_options(
        AllocatorGroupSpec {
            free_blocks: TEST_FREE_BLOCKS,
            free_inodes: TEST_FREE_INODES,
            used_directories: 0,
            flags: 0,
            block_bitmap: [0x3f, 0, 0, 0],
            inode_bitmap: [0xff, 0x03, 0, 0],
        },
        true,
        false,
        false,
    );
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    set_allocator_feature_bits(
        &mut filesystem,
        features::CompatFeatures::ORPHAN_FILE,
        features::ReadOnlyCompatFeatures::empty(),
    );
    (filesystem, inode)
}

/// Reopen through the real orphan-file loader after creating its backing inode.
pub(crate) fn recovery_orphan_file_test_filesystem() -> (Ext4SbInfo, Arc<TestDevice>) {
    let (mut filesystem, device) = recovery_test_filesystem();
    let inode = allocate_checkpointed_regular_inode(&mut filesystem);
    let mut bytes = vec![0; TEST_BLOCK_SIZE];
    put_u32(&mut bytes, TEST_BLOCK_SIZE - 8, 0x0b10_ca04);
    inode.set_size(TEST_BLOCK_SIZE as u64);
    filesystem
        .writeback_ordered_at(
            &inode,
            0,
            &bytes,
            TEST_BLOCK_SIZE as u64,
            crate::Ext4Timestamp::new(127, 0),
            crate::Ext4SyncIntent::FullMetadata,
        )
        .unwrap();
    filesystem.sync_filesystem().unwrap();
    drop(filesystem);
    {
        let mut image = lock(&device.bytes);
        let sb = &mut image[1024..2048];
        let compat = le_u32(sb, 0x5c) | features::CompatFeatures::ORPHAN_FILE.bits();
        put_u32(sb, 0x5c, compat);
        put_u32(sb, 0x280, inode.number().get());
    }
    let filesystem = Ext4SbInfo::mount(device.clone()).unwrap();
    (filesystem, device)
}

// ---- shared prealloc-space test helpers ----

/// Seeds one prealloc space into both index sides, keeping the conservative
/// counter in step. Blocks at `phys_start..phys_start+phys_len` must be free
/// on the bitmap (otherwise consume will fail the allocation).
pub(crate) fn insert_prealloc_space(
    filesystem: &Ext4SbInfo,
    inode: &Arc<Ext4Inode>,
    logical_start: u64,
    phys_start: u64,
    phys_len: u32,
    group: u32,
) {
    let mut alloc = lock(&filesystem.allocator);
    let space = PreallocSpace {
        group: BlockGroupNumber::new(group),
        logical_start: LogicalBlock::new(logical_start),
        phys_start: FilesystemBlock::new(phys_start),
        phys_len: BlockCount::new(phys_len),
    };
    inode.prealloc_map().update(|preallocs| {
        preallocs.insert(space.logical_start, space);
    });
    alloc.preallocs_by_group.insert(
        (space.group, space.phys_start),
        GroupPreallocSpace {
            logical_start: space.logical_start,
            phys_len: space.phys_len,
            owner: inode.clone(),
        },
    );
    alloc.preallocated_blocks += u64::from(phys_len);
}

/// Returns how many prealloc spaces this inode holds.
pub(crate) fn prealloc_count(inode: &Ext4Inode) -> usize {
    inode.prealloc_map().update(|preallocs| preallocs.len())
}

/// Returns how many group-side twin entries exist.
pub(crate) fn prealloc_twin_count(filesystem: &Ext4SbInfo) -> usize {
    lock(&filesystem.allocator).preallocs_by_group.len()
}
