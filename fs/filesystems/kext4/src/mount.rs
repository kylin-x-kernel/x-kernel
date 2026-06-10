// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec, vec::Vec};

use block::BlockDevice;

use crate::{
    BlockGroupNumber, ChecksumTarget, CorruptKind, Ext4Error, Ext4Result, FilesystemBlock,
    FilesystemDevice,
    disk::{BlockGroupDescriptor, Superblock, checksum, features, superblock},
};

/// Immutable geometry derived from a validated ext4 superblock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FilesystemLayout {
    block_size: u32,
    block_count: u64,
    group_count: u32,
    descriptor_size: u16,
    descriptor_table_start: FilesystemBlock,
    inode_table_blocks_per_group: u32,
}

impl FilesystemLayout {
    fn derive(superblock: &Superblock) -> Ext4Result<Self> {
        let data_blocks = superblock
            .blocks_count()
            .checked_sub(u64::from(superblock.first_data_block()))
            .ok_or(Ext4Error::Corrupt(CorruptKind::ZeroGeometry))?;
        let blocks_per_group = u64::from(superblock.blocks_per_group());
        let group_count = data_blocks
            .checked_add(blocks_per_group - 1)
            .ok_or(Ext4Error::Overflow)?
            / blocks_per_group;
        let group_count = u32::try_from(group_count).map_err(|_| Ext4Error::Overflow)?;

        let inode_table_bytes = u64::from(superblock.inodes_per_group())
            .checked_mul(u64::from(superblock.inode_size()))
            .ok_or(Ext4Error::Overflow)?;
        let block_size = u64::from(superblock.block_size());
        let inode_table_blocks_per_group = inode_table_bytes
            .checked_add(block_size - 1)
            .ok_or(Ext4Error::Overflow)?
            / block_size;
        let inode_table_blocks_per_group =
            u32::try_from(inode_table_blocks_per_group).map_err(|_| Ext4Error::Overflow)?;

        Ok(Self {
            block_size: superblock.block_size(),
            block_count: superblock.blocks_count(),
            group_count,
            descriptor_size: superblock.descriptor_size(),
            descriptor_table_start: FilesystemBlock::new(if superblock.block_size() == 1024 {
                2
            } else {
                1
            }),
            inode_table_blocks_per_group,
        })
    }

    /// Returns the filesystem block size in bytes.
    pub const fn block_size(self) -> u32 {
        self.block_size
    }

    /// Returns the total filesystem block count.
    pub const fn block_count(self) -> u64 {
        self.block_count
    }

    /// Returns the number of block groups.
    pub const fn group_count(self) -> u32 {
        self.group_count
    }

    /// Returns the size of one block group descriptor.
    pub const fn descriptor_size(self) -> u16 {
        self.descriptor_size
    }

    /// Returns the first block of the primary group descriptor table.
    pub const fn descriptor_table_start(self) -> FilesystemBlock {
        self.descriptor_table_start
    }

    /// Returns the number of inode table blocks in each group.
    pub const fn inode_table_blocks_per_group(self) -> u32 {
        self.inode_table_blocks_per_group
    }
}

/// A validated, read-only view of ext4 storage metadata.
pub struct ReadOnlyFilesystem {
    device: FilesystemDevice,
    superblock: Superblock,
    layout: FilesystemLayout,
    groups: Vec<BlockGroupDescriptor>,
}

impl ReadOnlyFilesystem {
    /// Reads and validates the primary ext4 superblock and group descriptor table.
    pub fn mount(device: Arc<dyn BlockDevice>) -> Ext4Result<Self> {
        let mut superblock_bytes = [0; superblock::SUPERBLOCK_SIZE];
        FilesystemDevice::read_bytes(
            device.as_ref(),
            superblock::SUPERBLOCK_OFFSET,
            &mut superblock_bytes,
        )?;
        let superblock = Superblock::decode(&superblock_bytes)?;
        let layout = FilesystemLayout::derive(&superblock)?;
        let filesystem_device = FilesystemDevice::open(
            device,
            usize::try_from(layout.block_size).map_err(|_| Ext4Error::Overflow)?,
            layout.block_count,
        )?;

        let descriptor_bytes = usize::try_from(layout.group_count)
            .map_err(|_| Ext4Error::Overflow)?
            .checked_mul(usize::from(layout.descriptor_size))
            .ok_or(Ext4Error::Overflow)?;
        let block_size = usize::try_from(layout.block_size).map_err(|_| Ext4Error::Overflow)?;
        let descriptor_blocks = descriptor_bytes
            .checked_add(block_size - 1)
            .ok_or(Ext4Error::Overflow)?
            / block_size;
        let mut table = vec![
            0;
            descriptor_blocks
                .checked_mul(block_size)
                .ok_or(Ext4Error::Overflow)?
        ];
        filesystem_device.read_blocks(
            layout.descriptor_table_start,
            u32::try_from(descriptor_blocks).map_err(|_| Ext4Error::Overflow)?,
            &mut table,
        )?;

        let mut groups = Vec::with_capacity(
            usize::try_from(layout.group_count).map_err(|_| Ext4Error::Overflow)?,
        );
        for group in 0..layout.group_count {
            let start = usize::try_from(group)
                .map_err(|_| Ext4Error::Overflow)?
                .checked_mul(usize::from(layout.descriptor_size))
                .ok_or(Ext4Error::Overflow)?;
            let end = start
                .checked_add(usize::from(layout.descriptor_size))
                .ok_or(Ext4Error::Overflow)?;
            let encoded = table.get(start..end).ok_or(Ext4Error::OutOfBounds)?;
            let descriptor =
                BlockGroupDescriptor::decode(encoded, superblock.features().has_64bit())?;

            if superblock.features().has_metadata_checksum() {
                let computed =
                    checksum::group_descriptor_checksum(encoded, group, superblock.checksum_seed())
                        .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
                if computed != descriptor.checksum() {
                    return Err(Ext4Error::ChecksumMismatch {
                        target: ChecksumTarget::BlockGroup(group),
                        expected: u32::from(computed),
                        actual: u32::from(descriptor.checksum()),
                    });
                }
            }
            if !superblock.features().has_metadata_checksum()
                && superblock.features().read_only_compat() & features::RO_COMPAT_GDT_CSUM != 0
            {
                // TODO: Implement the legacy UUID-based CRC16 GDT checksum.
                return Err(Ext4Error::UnsupportedFeature {
                    class: crate::FeatureClass::ReadOnlyCompatible,
                    bits: features::RO_COMPAT_GDT_CSUM,
                });
            }

            validate_group(&superblock, &layout, group, &descriptor)?;
            groups.push(descriptor);
        }

        Ok(Self {
            device: filesystem_device,
            superblock,
            layout,
            groups,
        })
    }

    /// Returns the decoded primary superblock.
    pub const fn superblock(&self) -> &Superblock {
        &self.superblock
    }

    /// Returns the immutable derived layout.
    pub const fn layout(&self) -> FilesystemLayout {
        self.layout
    }

    /// Returns a validated block group descriptor.
    pub fn group(&self, group: BlockGroupNumber) -> Option<&BlockGroupDescriptor> {
        self.groups.get(group.get() as usize)
    }

    /// Returns all validated block group descriptors.
    pub fn groups(&self) -> &[BlockGroupDescriptor] {
        &self.groups
    }

    /// Reads complete filesystem blocks without exposing metadata mutation.
    pub fn read_blocks(
        &self,
        start: FilesystemBlock,
        block_count: u32,
        output: &mut [u8],
    ) -> Ext4Result<()> {
        self.device.read_blocks(start, block_count, output)
    }
}

fn validate_group(
    superblock: &Superblock,
    layout: &FilesystemLayout,
    group: u32,
    descriptor: &BlockGroupDescriptor,
) -> Ext4Result<()> {
    let first = u64::from(superblock.first_data_block())
        .checked_add(
            u64::from(group)
                .checked_mul(u64::from(superblock.blocks_per_group()))
                .ok_or(Ext4Error::Overflow)?,
        )
        .ok_or(Ext4Error::Overflow)?;
    let end = first
        .checked_add(u64::from(superblock.blocks_per_group()))
        .ok_or(Ext4Error::Overflow)?
        .min(superblock.blocks_count());

    let in_group = |block: u64| block >= first && block < end;
    let inode_table_end = descriptor
        .inode_table()
        .checked_add(u64::from(layout.inode_table_blocks_per_group))
        .ok_or(Ext4Error::Overflow)?;
    let is_valid = if superblock.features().has_flex_bg() {
        let groups_per_flex = 1u32
            .checked_shl(u32::from(superblock.log_groups_per_flex()))
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidFlexGeometry))?;
        let flex_first_group = group / groups_per_flex * groups_per_flex;
        let flex_first = u64::from(superblock.first_data_block())
            .checked_add(
                u64::from(flex_first_group)
                    .checked_mul(u64::from(superblock.blocks_per_group()))
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?;
        let flex_group_count = groups_per_flex.min(layout.group_count - flex_first_group);
        let flex_end = flex_first
            .checked_add(
                u64::from(flex_group_count)
                    .checked_mul(u64::from(superblock.blocks_per_group()))
                    .ok_or(Ext4Error::Overflow)?,
            )
            .ok_or(Ext4Error::Overflow)?
            .min(superblock.blocks_count());
        let in_flex = |block: u64| block >= flex_first && block < flex_end;

        in_flex(descriptor.block_bitmap())
            && in_flex(descriptor.inode_bitmap())
            && descriptor.inode_table() >= flex_first
            && inode_table_end <= flex_end
    } else {
        in_group(descriptor.block_bitmap())
            && in_group(descriptor.inode_bitmap())
            && descriptor.inode_table() >= first
            && inode_table_end <= end
    };
    if !is_valid {
        return Err(Ext4Error::Corrupt(CorruptKind::MetadataOutsideGroup));
    }
    Ok(())
}
