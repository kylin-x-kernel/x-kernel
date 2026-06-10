// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{
    ChecksumTarget, CorruptKind, Ext4Error, Ext4Result,
    disk::{checksum, codec, features::FeatureSet},
};

pub(crate) const SUPERBLOCK_SIZE: usize = 1024;
pub(crate) const SUPERBLOCK_OFFSET: u64 = 1024;

const EXT4_MAGIC: u16 = 0xef53;
const CHECKSUM_OFFSET: usize = 0x3fc;
const CRC32C_CHECKSUM_TYPE: u8 = 1;
const MIN_64BIT_DESCRIPTOR_SIZE: u16 = 64;
const MAX_DESCRIPTOR_SIZE: u16 = 1024;

/// Decoded fields required to derive the initial ext4 storage layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Superblock {
    inodes_count: u32,
    blocks_count: u64,
    first_data_block: u32,
    block_size: u32,
    blocks_per_group: u32,
    clusters_per_group: u32,
    inodes_per_group: u32,
    inode_size: u16,
    descriptor_size: u16,
    log_groups_per_flex: u8,
    features: FeatureSet,
    uuid: [u8; 16],
    checksum_seed: u32,
    journal_inode: u32,
}

impl Superblock {
    pub(crate) fn decode(input: &[u8]) -> Ext4Result<Self> {
        if input.len() != SUPERBLOCK_SIZE {
            return Err(Ext4Error::InvalidBufferLength {
                expected: SUPERBLOCK_SIZE,
                actual: input.len(),
            });
        }

        let magic = codec::le_u16(input, 0x38)?;
        if magic != EXT4_MAGIC {
            return Err(Ext4Error::InvalidMagic(magic));
        }

        let revision = codec::le_u32(input, 0x4c)?;
        if revision != 1 {
            // TODO: Support Linux ext4's revision 0 compatibility path.
            return Err(Ext4Error::UnsupportedRevision(revision));
        }

        let log_block_size = codec::le_u32(input, 0x18)?;
        let block_size = 1024u32
            .checked_shl(log_block_size)
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockSize))?;
        if !(1024..=65_536).contains(&block_size) {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockSize));
        }

        let features = FeatureSet::new(
            codec::le_u32(input, 0x5c)?,
            codec::le_u32(input, 0x60)?,
            codec::le_u32(input, 0x64)?,
        );
        features.validate_read_only()?;

        if features.has_metadata_checksum() {
            if input[0x175] != CRC32C_CHECKSUM_TYPE {
                return Err(Ext4Error::UnsupportedFeature {
                    class: crate::FeatureClass::ReadOnlyCompatible,
                    bits: features.read_only_compat(),
                });
            }
            let expected = checksum::crc32c(u32::MAX, &input[..CHECKSUM_OFFSET]);
            let actual = codec::le_u32(input, CHECKSUM_OFFSET)?;
            if expected != actual {
                return Err(Ext4Error::ChecksumMismatch {
                    target: ChecksumTarget::Superblock,
                    expected,
                    actual,
                });
            }
        }

        let blocks_count = u64::from(codec::le_u32(input, 0x04)?)
            | if features.has_64bit() {
                u64::from(codec::le_u32(input, 0x150)?) << 32
            } else {
                0
            };
        let first_data_block = codec::le_u32(input, 0x14)?;
        let blocks_per_group = codec::le_u32(input, 0x20)?;
        let clusters_per_group = codec::le_u32(input, 0x24)?;
        let inodes_count = codec::le_u32(input, 0x00)?;
        let inodes_per_group = codec::le_u32(input, 0x28)?;
        if blocks_count == 0 || blocks_per_group == 0 || inodes_per_group == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::ZeroGeometry));
        }
        if u64::from(first_data_block) >= blocks_count
            || (block_size == 1024 && first_data_block == 0)
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        let bitmap_capacity = block_size.checked_mul(8).ok_or(Ext4Error::Overflow)?;
        if blocks_per_group > bitmap_capacity {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        if codec::le_u32(input, 0x1c)? != log_block_size
            || clusters_per_group != blocks_per_group
            || clusters_per_group > bitmap_capacity
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidClusterGeometry));
        }

        let inode_size = codec::le_u16(input, 0x58)?;
        if inode_size < 128 || !inode_size.is_power_of_two() || u32::from(inode_size) > block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeSize));
        }
        let inodes_per_block = block_size / u32::from(inode_size);
        if inodes_per_group < inodes_per_block || inodes_per_group > bitmap_capacity {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry));
        }

        let descriptor_size = if features.has_64bit() {
            codec::le_u16(input, 0xfe)?
        } else {
            32
        };
        if features.has_64bit()
            && (!(MIN_64BIT_DESCRIPTOR_SIZE..=MAX_DESCRIPTOR_SIZE).contains(&descriptor_size)
                || !descriptor_size.is_power_of_two()
                || u32::from(descriptor_size) > block_size)
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidDescriptorSize));
        }

        let data_blocks = blocks_count
            .checked_sub(u64::from(first_data_block))
            .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))?;
        let group_count = data_blocks
            .checked_add(u64::from(blocks_per_group) - 1)
            .ok_or(Ext4Error::Overflow)?
            / u64::from(blocks_per_group);
        if group_count
            .checked_mul(u64::from(inodes_per_group))
            .ok_or(Ext4Error::Overflow)?
            != u64::from(inodes_count)
        {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry));
        }

        let uuid = codec::bytes(input, 0x68)?;
        let checksum_seed = if features.has_checksum_seed() {
            codec::le_u32(input, 0x270)?
        } else {
            checksum::crc32c(u32::MAX, &uuid)
        };
        let log_groups_per_flex = input[0x174];
        if features.has_flex_bg() && 1u32.checked_shl(u32::from(log_groups_per_flex)).is_none() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidFlexGeometry));
        }

        Ok(Self {
            inodes_count,
            blocks_count,
            first_data_block,
            block_size,
            blocks_per_group,
            clusters_per_group,
            inodes_per_group,
            inode_size,
            descriptor_size,
            log_groups_per_flex,
            features,
            uuid,
            checksum_seed,
            journal_inode: codec::le_u32(input, 0xe0)?,
        })
    }

    /// Returns the total inode count.
    pub const fn inodes_count(&self) -> u32 {
        self.inodes_count
    }

    /// Returns the total filesystem block count.
    pub const fn blocks_count(&self) -> u64 {
        self.blocks_count
    }

    /// Returns the first data block.
    pub const fn first_data_block(&self) -> u32 {
        self.first_data_block
    }

    /// Returns the filesystem block size in bytes.
    pub const fn block_size(&self) -> u32 {
        self.block_size
    }

    /// Returns the number of blocks per block group.
    pub const fn blocks_per_group(&self) -> u32 {
        self.blocks_per_group
    }

    /// Returns the number of clusters per block group.
    pub const fn clusters_per_group(&self) -> u32 {
        self.clusters_per_group
    }

    /// Returns the number of inodes per block group.
    pub const fn inodes_per_group(&self) -> u32 {
        self.inodes_per_group
    }

    /// Returns the on-disk inode size.
    pub const fn inode_size(&self) -> u16 {
        self.inode_size
    }

    /// Returns the block group descriptor size.
    pub const fn descriptor_size(&self) -> u16 {
        self.descriptor_size
    }

    /// Returns log2 of the number of block groups in a flex group.
    pub const fn log_groups_per_flex(&self) -> u8 {
        self.log_groups_per_flex
    }

    /// Returns the negotiated feature bitmaps.
    pub const fn features(&self) -> FeatureSet {
        self.features
    }

    /// Returns the filesystem UUID.
    pub const fn uuid(&self) -> [u8; 16] {
        self.uuid
    }

    pub(crate) const fn checksum_seed(&self) -> u32 {
        self.checksum_seed
    }

    /// Returns the inode number of the internal journal.
    pub const fn journal_inode(&self) -> u32 {
        self.journal_inode
    }
}

#[cfg(test)]
mod tests {
    use super::{SUPERBLOCK_SIZE, Superblock};
    use crate::{CorruptKind, Ext4Error};

    const INCOMPAT_EXTENTS: u32 = 0x0040;
    const INCOMPAT_64BIT: u32 = 0x0080;

    #[test]
    fn accepts_linux_64bit_descriptor_geometry() {
        for descriptor_size in [64, 128, 1024] {
            let mut bytes = valid_superblock();
            put_u16(&mut bytes, 0xfe, descriptor_size);
            assert!(Superblock::decode(&bytes).is_ok());
        }
    }

    #[test]
    fn rejects_non_power_of_two_64bit_descriptor_sizes() {
        for descriptor_size in [32, 40, 48, 56, 96, 1025] {
            let mut bytes = valid_superblock();
            put_u16(&mut bytes, 0xfe, descriptor_size);
            assert_eq!(
                Superblock::decode(&bytes),
                Err(Ext4Error::Corrupt(CorruptKind::InvalidDescriptorSize))
            );
        }
    }

    #[test]
    fn rejects_first_data_block_outside_filesystem() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x14, 32_768);
        assert_eq!(
            Superblock::decode(&bytes),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))
        );
    }

    #[test]
    fn rejects_zero_first_data_block_for_one_kib_blocks() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x18, 0);
        put_u32(&mut bytes, 0x1c, 0);
        assert_eq!(
            Superblock::decode(&bytes),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))
        );
    }

    #[test]
    fn rejects_group_geometry_larger_than_bitmap() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x20, 32_769);
        put_u32(&mut bytes, 0x24, 32_769);
        assert_eq!(
            Superblock::decode(&bytes),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry))
        );
    }

    #[test]
    fn rejects_cluster_geometry_larger_than_bitmap() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x24, 32_769);
        assert_eq!(
            Superblock::decode(&bytes),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidClusterGeometry))
        );
    }

    #[test]
    fn rejects_invalid_inode_geometry() {
        let mut too_few = valid_superblock();
        put_u32(&mut too_few, 0x00, 8);
        put_u32(&mut too_few, 0x28, 8);
        assert_eq!(
            Superblock::decode(&too_few),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry))
        );

        let mut too_many = valid_superblock();
        put_u32(&mut too_many, 0x00, 32_769);
        put_u32(&mut too_many, 0x28, 32_769);
        assert_eq!(
            Superblock::decode(&too_many),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry))
        );

        let mut count_mismatch = valid_superblock();
        put_u32(&mut count_mismatch, 0x00, 8191);
        assert_eq!(
            Superblock::decode(&count_mismatch),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry))
        );
    }

    fn valid_superblock() -> [u8; SUPERBLOCK_SIZE] {
        let mut bytes = [0; SUPERBLOCK_SIZE];
        put_u32(&mut bytes, 0x00, 8192);
        put_u32(&mut bytes, 0x04, 32_768);
        put_u32(&mut bytes, 0x14, 0);
        put_u32(&mut bytes, 0x18, 2);
        put_u32(&mut bytes, 0x1c, 2);
        put_u32(&mut bytes, 0x20, 32_768);
        put_u32(&mut bytes, 0x24, 32_768);
        put_u32(&mut bytes, 0x28, 8192);
        put_u16(&mut bytes, 0x38, 0xef53);
        put_u32(&mut bytes, 0x4c, 1);
        put_u16(&mut bytes, 0x58, 256);
        put_u32(&mut bytes, 0x60, INCOMPAT_EXTENTS | INCOMPAT_64BIT);
        put_u16(&mut bytes, 0xfe, 64);
        bytes
    }

    fn put_u16(output: &mut [u8], offset: usize, value: u16) {
        output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(output: &mut [u8], offset: usize, value: u32) {
        output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}
