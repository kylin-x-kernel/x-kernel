// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::num::NonZeroU32;

use crate::{
    ChecksumTarget, CorruptKind, Ext4Error, Ext4Result,
    disk::{
        checksum, codec,
        features::{self, FeatureSet},
    },
};

pub(crate) const SUPERBLOCK_SIZE: usize = 1024;
pub(crate) const SUPERBLOCK_OFFSET: u64 = 1024;

const EXT4_MAGIC: u16 = 0xef53;
pub(crate) const EXT2_FLAGS_SIGNED_HASH: u32 = 0x0001;
pub(crate) const EXT2_FLAGS_UNSIGNED_HASH: u32 = 0x0002;
const CHECKSUM_OFFSET: usize = 0x3fc;
const CRC32C_CHECKSUM_TYPE: u8 = 1;
const FEATURE_INCOMPAT_OFFSET: usize = 0x60;
const FREE_BLOCKS_COUNT_LO_OFFSET: usize = 0x0c;
const FREE_BLOCKS_COUNT_HI_OFFSET: usize = 0x158;
const FREE_INODES_COUNT_OFFSET: usize = 0x10;
const LAST_ORPHAN_OFFSET: usize = 0xe8;
const MIN_EXTRA_ISIZE_OFFSET: usize = 0x15c;
const WANT_EXTRA_ISIZE_OFFSET: usize = 0x15e;
const FLAGS_OFFSET: usize = 0x160;
// Linux initializes `s_want_extra_isize` to the fields currently known by
// `struct ext4_inode` before considering the on-disk min/want values.
const LINUX_DEFAULT_EXTRA_ISIZE: u16 = 32;
const MIN_64BIT_DESCRIPTOR_SIZE: u16 = 64;
const MAX_DESCRIPTOR_SIZE: u16 = 1024;
const GOOD_OLD_FIRST_INODE: u32 = 11;

/// Ext4 fields that locate and back up the filesystem journal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct JournalFields {
    inode: Option<NonZeroU32>,
    device: Option<NonZeroU32>,
    uuid: [u8; 16],
    inode_backup: [u8; 68],
}

impl JournalFields {
    /// Returns the internal journal inode number, when encoded.
    pub const fn inode(self) -> Option<NonZeroU32> {
        self.inode
    }

    /// Returns the encoded external journal device, when encoded.
    pub const fn device(self) -> Option<NonZeroU32> {
        self.device
    }

    /// Returns the journal UUID recorded by ext4.
    pub const fn uuid(self) -> [u8; 16] {
        self.uuid
    }

    /// Returns the on-disk backup of the journal inode.
    pub const fn inode_backup(self) -> [u8; 68] {
        self.inode_backup
    }
}

/// Decoded fields required to derive the initial ext4 storage layout.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Superblock {
    inodes_count: u32,
    blocks_count: u64,
    reserved_blocks_count: u64,
    free_blocks_count: u64,
    free_inodes_count: u32,
    last_orphan: u32,
    first_data_block: u32,
    block_size: u32,
    blocks_per_group: u32,
    clusters_per_group: u32,
    inodes_per_group: u32,
    first_inode: u32,
    inode_size: u16,
    descriptor_size: u16,
    reserved_gdt_blocks: u16,
    log_groups_per_flex: u8,
    min_extra_isize: u16,
    effective_want_extra_isize: u16,
    features: FeatureSet,
    uuid: [u8; 16],
    checksum_seed: u32,
    hash_seed: [u32; 4],
    default_hash_version: u8,
    flags: u32,
    journal: JournalFields,
    backup_groups: [u32; 2],
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

        if features.has_metadata_checksum() {
            if input[0x175] != CRC32C_CHECKSUM_TYPE {
                return Err(Ext4Error::UnsupportedFeature {
                    class: crate::FeatureClass::ReadOnlyCompatible,
                    bits: features.read_only_compat().bits(),
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
        features.validate_read_only()?;

        let blocks_count = u64::from(codec::le_u32(input, 0x04)?)
            | if features.has_64bit() {
                u64::from(codec::le_u32(input, 0x150)?) << 32
            } else {
                0
            };
        let reserved_blocks_count = u64::from(codec::le_u32(input, 0x08)?)
            | if features.has_64bit() {
                u64::from(codec::le_u32(input, 0x154)?) << 32
            } else {
                0
            };
        let free_blocks_count = u64::from(codec::le_u32(input, FREE_BLOCKS_COUNT_LO_OFFSET)?)
            | if features.has_64bit() {
                u64::from(codec::le_u32(input, FREE_BLOCKS_COUNT_HI_OFFSET)?) << 32
            } else {
                0
            };
        let free_inodes_count = codec::le_u32(input, FREE_INODES_COUNT_OFFSET)?;
        let first_data_block = codec::le_u32(input, 0x14)?;
        let blocks_per_group = codec::le_u32(input, 0x20)?;
        let clusters_per_group = codec::le_u32(input, 0x24)?;
        let inodes_count = codec::le_u32(input, 0x00)?;
        let last_orphan = codec::le_u32(input, LAST_ORPHAN_OFFSET)?;
        let inodes_per_group = codec::le_u32(input, 0x28)?;
        if blocks_count == 0 || blocks_per_group == 0 || inodes_per_group == 0 {
            return Err(Ext4Error::Corrupt(CorruptKind::ZeroGeometry));
        }
        if free_blocks_count > blocks_count {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
        }
        if free_inodes_count > inodes_count {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry));
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

        let first_inode = codec::le_u32(input, 0x54)?;
        if first_inode < GOOD_OLD_FIRST_INODE {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry));
        }

        let inode_size = codec::le_u16(input, 0x58)?;
        if inode_size < 128 || !inode_size.is_power_of_two() || u32::from(inode_size) > block_size {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeSize));
        }
        let (min_extra_isize, disk_want_extra_isize) = if features.has_extra_isize() {
            let min_extra_isize = codec::le_u16(input, MIN_EXTRA_ISIZE_OFFSET)?;
            let disk_want_extra_isize = codec::le_u16(input, WANT_EXTRA_ISIZE_OFFSET)?;
            validate_extra_isize(inode_size, min_extra_isize)?;
            validate_extra_isize(inode_size, disk_want_extra_isize)?;
            (min_extra_isize, disk_want_extra_isize)
        } else {
            (0, 0)
        };
        let effective_want_extra_isize =
            effective_want_extra_isize(inode_size, min_extra_isize, disk_want_extra_isize);
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
        let hash_seed = [
            codec::le_u32(input, 0xec)?,
            codec::le_u32(input, 0xf0)?,
            codec::le_u32(input, 0xf4)?,
            codec::le_u32(input, 0xf8)?,
        ];
        let default_hash_version = *input
            .get(0xfc)
            .ok_or(Ext4Error::Corrupt(CorruptKind::Truncated))?;
        let flags = codec::le_u32(input, FLAGS_OFFSET)?;
        let log_groups_per_flex = input[0x174];
        if features.has_flex_bg() && 1u32.checked_shl(u32::from(log_groups_per_flex)).is_none() {
            return Err(Ext4Error::Corrupt(CorruptKind::InvalidFlexGeometry));
        }

        Ok(Self {
            inodes_count,
            blocks_count,
            reserved_blocks_count,
            free_blocks_count,
            free_inodes_count,
            last_orphan,
            first_data_block,
            block_size,
            blocks_per_group,
            clusters_per_group,
            inodes_per_group,
            first_inode,
            inode_size,
            descriptor_size,
            reserved_gdt_blocks: codec::le_u16(input, 0xce)?,
            log_groups_per_flex,
            min_extra_isize,
            effective_want_extra_isize,
            features,
            uuid,
            checksum_seed,
            hash_seed,
            default_hash_version,
            flags,
            journal: JournalFields {
                inode: NonZeroU32::new(codec::le_u32(input, 0xe0)?),
                device: NonZeroU32::new(codec::le_u32(input, 0xe4)?),
                uuid: codec::bytes(input, 0xd0)?,
                inode_backup: codec::bytes(input, 0x10c)?,
            },
            backup_groups: [codec::le_u32(input, 0x244)?, codec::le_u32(input, 0x248)?],
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

    /// Returns blocks reserved for privileged allocation.
    pub const fn reserved_blocks_count(&self) -> u64 {
        self.reserved_blocks_count
    }

    /// Returns the free block count decoded from the on-disk superblock.
    ///
    /// Mount-time snapshot: the live free-block aggregate lives in
    /// `Ext4SbInfo::free_blocks_count()` under the allocator lock. Use
    /// that accessor for anything that runs after mount.
    pub const fn on_disk_free_blocks_count(&self) -> u64 {
        self.free_blocks_count
    }

    /// Returns the free inode count decoded from the on-disk superblock.
    ///
    /// Mount-time snapshot: the live free-inode aggregate lives in
    /// `Ext4SbInfo::free_inodes_count()` under the allocator lock. Use
    /// that accessor for anything that runs after mount.
    pub const fn on_disk_free_inodes_count(&self) -> u32 {
        self.free_inodes_count
    }

    /// Returns the legacy ext4 orphan-list head inode number, or zero.
    pub const fn last_orphan(&self) -> u32 {
        self.last_orphan
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

    /// Returns the first non-reserved inode number.
    pub const fn first_inode(&self) -> u32 {
        self.first_inode
    }

    /// Returns the on-disk inode size.
    pub const fn inode_size(&self) -> u16 {
        self.inode_size
    }

    /// Returns the block group descriptor size.
    pub const fn descriptor_size(&self) -> u16 {
        self.descriptor_size
    }

    /// Returns the number of reserved group descriptor blocks.
    pub const fn reserved_gdt_blocks(&self) -> u16 {
        self.reserved_gdt_blocks
    }

    /// Returns log2 of the number of block groups in a flex group.
    pub const fn log_groups_per_flex(&self) -> u8 {
        self.log_groups_per_flex
    }

    /// Returns the minimum extra inode bytes required by this filesystem.
    pub const fn min_extra_isize(&self) -> u16 {
        self.min_extra_isize
    }

    /// Returns Linux's effective preferred extra inode bytes.
    ///
    /// The runtime value includes the 32 bytes reserved for fields known to
    /// Linux and, when `RO_COMPAT_EXTRA_ISIZE` is set, the filesystem's
    /// on-disk min/want requirements.
    pub const fn want_extra_isize(&self) -> u16 {
        self.effective_want_extra_isize
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

    pub(crate) const fn hash_seed(&self) -> [u32; 4] {
        self.hash_seed
    }

    pub(crate) const fn default_hash_version(&self) -> u8 {
        self.default_hash_version
    }

    pub(crate) const fn flags(&self) -> u32 {
        self.flags
    }

    /// Returns the fields that locate the filesystem journal.
    pub const fn journal(&self) -> JournalFields {
        self.journal
    }

    /// Returns the sparse_super2 backup group numbers.
    pub const fn backup_groups(&self) -> [u32; 2] {
        self.backup_groups
    }
}

pub(crate) fn clear_ext4_needs_recovery_feature(input: &mut [u8]) -> Ext4Result<()> {
    let superblock = Superblock::decode(input)?;
    let mut incompat = features::IncompatFeatures::from_bits_retain(codec::le_u32(
        input,
        FEATURE_INCOMPAT_OFFSET,
    )?);
    incompat.remove(features::IncompatFeatures::RECOVER);
    input[FEATURE_INCOMPAT_OFFSET..FEATURE_INCOMPAT_OFFSET + 4]
        .copy_from_slice(&incompat.bits().to_le_bytes());
    update_feature_checksum(input, &superblock)
}

pub(crate) fn set_ext4_needs_recovery_feature(input: &mut [u8]) -> Ext4Result<()> {
    let superblock = Superblock::decode(input)?;
    let mut incompat = features::IncompatFeatures::from_bits_retain(codec::le_u32(
        input,
        FEATURE_INCOMPAT_OFFSET,
    )?);
    incompat.insert(features::IncompatFeatures::RECOVER);
    input[FEATURE_INCOMPAT_OFFSET..FEATURE_INCOMPAT_OFFSET + 4]
        .copy_from_slice(&incompat.bits().to_le_bytes());
    update_feature_checksum(input, &superblock)
}

pub(crate) fn decrement_free_blocks_count(input: &mut [u8], blocks: u32) -> Ext4Result<Superblock> {
    let superblock = Superblock::decode(input)?;
    let free_blocks_count = superblock
        .on_disk_free_blocks_count()
        .checked_sub(u64::from(blocks))
        .ok_or(Ext4Error::NoSpace)?;
    set_free_blocks_count(input, free_blocks_count)
}

pub(crate) fn increment_free_blocks_count(input: &mut [u8], blocks: u32) -> Ext4Result<Superblock> {
    let superblock = Superblock::decode(input)?;
    let free_blocks_count = superblock
        .on_disk_free_blocks_count()
        .checked_add(u64::from(blocks))
        .ok_or(Ext4Error::Overflow)?;
    set_free_blocks_count(input, free_blocks_count)
}

pub(crate) fn decrement_free_inodes_count(input: &mut [u8], inodes: u32) -> Ext4Result<Superblock> {
    let superblock = Superblock::decode(input)?;
    let free_inodes_count = superblock
        .on_disk_free_inodes_count()
        .checked_sub(inodes)
        .ok_or(Ext4Error::NoSpace)?;
    set_free_inodes_count(input, free_inodes_count)
}

pub(crate) fn increment_free_inodes_count(input: &mut [u8], inodes: u32) -> Ext4Result<Superblock> {
    let superblock = Superblock::decode(input)?;
    let free_inodes_count = superblock
        .on_disk_free_inodes_count()
        .checked_add(inodes)
        .ok_or(Ext4Error::Overflow)?;
    set_free_inodes_count(input, free_inodes_count)
}

pub(crate) fn set_last_orphan(input: &mut [u8], last_orphan: u32) -> Ext4Result<Superblock> {
    let superblock = Superblock::decode(input)?;
    if last_orphan > superblock.inodes_count() {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber));
    }

    input[LAST_ORPHAN_OFFSET..LAST_ORPHAN_OFFSET + 4].copy_from_slice(&last_orphan.to_le_bytes());
    update_feature_checksum(input, &superblock)?;
    Superblock::decode(input)
}

pub(crate) fn set_unsigned_hash_flag(input: &mut [u8]) -> Ext4Result<Superblock> {
    let superblock = Superblock::decode(input)?;
    let flags = superblock.flags() | EXT2_FLAGS_UNSIGNED_HASH;
    input[FLAGS_OFFSET..FLAGS_OFFSET + 4].copy_from_slice(&flags.to_le_bytes());
    update_feature_checksum(input, &superblock)?;
    Superblock::decode(input)
}

fn set_free_blocks_count(input: &mut [u8], free_blocks_count: u64) -> Ext4Result<Superblock> {
    let superblock = Superblock::decode(input)?;
    if free_blocks_count > superblock.blocks_count() {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidBlockGroupGeometry));
    }
    if !superblock.features().has_64bit() && free_blocks_count > u64::from(u32::MAX) {
        return Err(Ext4Error::Overflow);
    }

    let low = free_blocks_count as u32;
    input[FREE_BLOCKS_COUNT_LO_OFFSET..FREE_BLOCKS_COUNT_LO_OFFSET + 4]
        .copy_from_slice(&low.to_le_bytes());
    if superblock.features().has_64bit() {
        let high = (free_blocks_count >> 32) as u32;
        input[FREE_BLOCKS_COUNT_HI_OFFSET..FREE_BLOCKS_COUNT_HI_OFFSET + 4]
            .copy_from_slice(&high.to_le_bytes());
    }
    update_feature_checksum(input, &superblock)?;
    Superblock::decode(input)
}

fn set_free_inodes_count(input: &mut [u8], free_inodes_count: u32) -> Ext4Result<Superblock> {
    let superblock = Superblock::decode(input)?;
    if free_inodes_count > superblock.inodes_count() {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry));
    }

    input[FREE_INODES_COUNT_OFFSET..FREE_INODES_COUNT_OFFSET + 4]
        .copy_from_slice(&free_inodes_count.to_le_bytes());
    update_feature_checksum(input, &superblock)?;
    Superblock::decode(input)
}

fn update_feature_checksum(input: &mut [u8], superblock: &Superblock) -> Ext4Result<()> {
    if superblock.features().has_metadata_checksum() {
        let checksum = checksum::crc32c(u32::MAX, &input[..CHECKSUM_OFFSET]);
        input[CHECKSUM_OFFSET..CHECKSUM_OFFSET + 4].copy_from_slice(&checksum.to_le_bytes());
    }
    Ok(())
}

fn validate_extra_isize(inode_size: u16, extra_isize: u16) -> Ext4Result<()> {
    let available = inode_size
        .checked_sub(crate::disk::inode::GOOD_OLD_INODE_SIZE as u16)
        .ok_or(Ext4Error::Corrupt(CorruptKind::InvalidInodeSize))?;
    if extra_isize > available || !extra_isize.is_multiple_of(4) {
        return Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeGeometry));
    }
    Ok(())
}

fn effective_want_extra_isize(
    inode_size: u16,
    min_extra_isize: u16,
    disk_want_extra_isize: u16,
) -> u16 {
    let available = inode_size.saturating_sub(crate::disk::inode::GOOD_OLD_INODE_SIZE as u16);
    LINUX_DEFAULT_EXTRA_ISIZE
        .min(available)
        .max(min_extra_isize)
        .max(disk_want_extra_isize)
}

#[cfg(test)]
mod tests {
    use super::{
        CHECKSUM_OFFSET, CRC32C_CHECKSUM_TYPE, EXT2_FLAGS_UNSIGNED_HASH, LINUX_DEFAULT_EXTRA_ISIZE,
        MIN_EXTRA_ISIZE_OFFSET, SUPERBLOCK_SIZE, Superblock, WANT_EXTRA_ISIZE_OFFSET,
    };
    use crate::{ChecksumTarget, CorruptKind, Ext4Error, disk::checksum};

    const INCOMPAT_EXTENTS: u32 = 0x0040;
    const INCOMPAT_RECOVER: u32 = 0x0004;
    const INCOMPAT_64BIT: u32 = 0x0080;
    const RO_COMPAT_EXTRA_ISIZE: u32 = 0x0040;
    const RO_COMPAT_BIGALLOC: u32 = 0x0200;
    const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

    #[test]
    fn derives_linux_default_effective_want_extra_isize() {
        let decoded = Superblock::decode(&valid_superblock()).unwrap();

        assert_eq!(decoded.min_extra_isize(), 0);
        assert_eq!(decoded.want_extra_isize(), LINUX_DEFAULT_EXTRA_ISIZE);
    }

    #[test]
    fn effective_want_extra_isize_includes_disk_requirements() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x64, RO_COMPAT_EXTRA_ISIZE);
        put_u16(&mut bytes, MIN_EXTRA_ISIZE_OFFSET, 48);
        put_u16(&mut bytes, WANT_EXTRA_ISIZE_OFFSET, 64);

        let decoded = Superblock::decode(&bytes).unwrap();

        assert_eq!(decoded.min_extra_isize(), 48);
        assert_eq!(decoded.want_extra_isize(), 64);
    }

    #[test]
    fn effective_want_extra_isize_ignores_disk_fields_without_feature() {
        let mut bytes = valid_superblock();
        put_u16(&mut bytes, MIN_EXTRA_ISIZE_OFFSET, 48);
        put_u16(&mut bytes, WANT_EXTRA_ISIZE_OFFSET, 64);

        let decoded = Superblock::decode(&bytes).unwrap();

        assert_eq!(decoded.min_extra_isize(), 0);
        assert_eq!(decoded.want_extra_isize(), LINUX_DEFAULT_EXTRA_ISIZE);
    }

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
    fn rejects_bigalloc_before_cluster_semantics_can_be_mounted() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x64, RO_COMPAT_BIGALLOC);
        assert_eq!(
            Superblock::decode(&bytes),
            Err(Ext4Error::UnsupportedFeature {
                class: crate::FeatureClass::ReadOnlyCompatible,
                bits: RO_COMPAT_BIGALLOC,
            })
        );
    }

    #[test]
    fn decodes_non_extent_superblock_before_mount_negotiation() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x60, 0);

        assert!(Superblock::decode(&bytes).is_ok());
    }

    #[test]
    fn checksum_failure_precedes_mount_feature_negotiation() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x64, RO_COMPAT_METADATA_CSUM);
        bytes[0x175] = CRC32C_CHECKSUM_TYPE;
        update_superblock_checksum(&mut bytes);

        // Corrupt the checksummed feature word so the filesystem also appears
        // to lack extents. Integrity failure must win over mount capability.
        put_u32(&mut bytes, 0x60, INCOMPAT_64BIT);

        assert!(matches!(
            Superblock::decode(&bytes),
            Err(Ext4Error::ChecksumMismatch {
                target: ChecksumTarget::Superblock,
                ..
            })
        ));
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

    #[test]
    fn decodes_and_updates_legacy_orphan_head() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, super::LAST_ORPHAN_OFFSET, 12);

        let decoded = Superblock::decode(&bytes).unwrap();
        assert_eq!(decoded.last_orphan(), 12);

        let updated = super::set_last_orphan(&mut bytes, 13).unwrap();
        assert_eq!(updated.last_orphan(), 13);
        assert_eq!(Superblock::decode(&bytes).unwrap().last_orphan(), 13);
    }

    #[test]
    fn preserves_directory_hash_flags_as_disk_facts() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x160, EXT2_FLAGS_UNSIGNED_HASH);

        assert_eq!(
            Superblock::decode(&bytes)
                .expect("decode unsigned-hash superblock")
                .flags(),
            EXT2_FLAGS_UNSIGNED_HASH
        );
    }

    #[test]
    fn setting_unsigned_hash_flag_updates_metadata_checksum() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x64, RO_COMPAT_METADATA_CSUM);
        bytes[0x175] = CRC32C_CHECKSUM_TYPE;
        update_superblock_checksum(&mut bytes);

        let updated = super::set_unsigned_hash_flag(&mut bytes).unwrap();

        assert_eq!(
            updated.flags() & EXT2_FLAGS_UNSIGNED_HASH,
            EXT2_FLAGS_UNSIGNED_HASH
        );
        assert_eq!(Superblock::decode(&bytes).unwrap(), updated);
    }

    #[test]
    fn decodes_out_of_range_orphan_head_for_recovery() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, super::LAST_ORPHAN_OFFSET, 8193);

        let decoded = Superblock::decode(&bytes).unwrap();
        assert_eq!(decoded.last_orphan(), 8193);
        assert_eq!(
            super::set_last_orphan(&mut bytes, 8193),
            Err(Ext4Error::Corrupt(CorruptKind::InvalidInodeNumber))
        );
        let cleared = super::set_last_orphan(&mut bytes, 0).unwrap();
        assert_eq!(cleared.last_orphan(), 0);
    }

    #[test]
    fn clears_recovery_feature_without_metadata_checksum() {
        let mut bytes = valid_superblock();
        put_u32(
            &mut bytes,
            0x60,
            INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_RECOVER,
        );

        super::clear_ext4_needs_recovery_feature(&mut bytes).unwrap();

        assert_eq!(
            Superblock::decode(&bytes)
                .unwrap()
                .features()
                .incompat()
                .bits()
                & INCOMPAT_RECOVER,
            0
        );
    }

    #[test]
    fn sets_recovery_feature_without_metadata_checksum() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x60, INCOMPAT_EXTENTS | INCOMPAT_64BIT);

        super::set_ext4_needs_recovery_feature(&mut bytes).unwrap();

        assert_ne!(
            Superblock::decode(&bytes)
                .unwrap()
                .features()
                .incompat()
                .bits()
                & INCOMPAT_RECOVER,
            0
        );
    }

    #[test]
    fn clears_recovery_feature_and_updates_metadata_checksum() {
        let mut bytes = valid_superblock();
        put_u32(
            &mut bytes,
            0x60,
            INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_RECOVER,
        );
        put_u32(&mut bytes, 0x64, RO_COMPAT_METADATA_CSUM);
        bytes[0x175] = CRC32C_CHECKSUM_TYPE;
        update_superblock_checksum(&mut bytes);

        super::clear_ext4_needs_recovery_feature(&mut bytes).unwrap();

        let decoded = Superblock::decode(&bytes).unwrap();
        assert_eq!(decoded.features().incompat().bits() & INCOMPAT_RECOVER, 0);
    }

    #[test]
    fn sets_recovery_feature_and_updates_metadata_checksum() {
        let mut bytes = valid_superblock();
        put_u32(&mut bytes, 0x60, INCOMPAT_EXTENTS | INCOMPAT_64BIT);
        put_u32(&mut bytes, 0x64, RO_COMPAT_METADATA_CSUM);
        bytes[0x175] = CRC32C_CHECKSUM_TYPE;
        update_superblock_checksum(&mut bytes);

        super::set_ext4_needs_recovery_feature(&mut bytes).unwrap();

        let decoded = Superblock::decode(&bytes).unwrap();
        assert_ne!(decoded.features().incompat().bits() & INCOMPAT_RECOVER, 0);
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
        put_u32(&mut bytes, 0x54, 11);
        put_u16(&mut bytes, 0x58, 256);
        put_u32(&mut bytes, 0x60, INCOMPAT_EXTENTS | INCOMPAT_64BIT);
        put_u16(&mut bytes, 0xfe, 64);
        bytes
    }

    fn update_superblock_checksum(bytes: &mut [u8]) {
        let checksum = checksum::crc32c(u32::MAX, &bytes[..CHECKSUM_OFFSET]);
        put_u32(bytes, CHECKSUM_OFFSET, checksum);
    }

    fn put_u16(output: &mut [u8], offset: usize, value: u16) {
        output[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
    }

    fn put_u32(output: &mut [u8], offset: usize, value: u32) {
        output[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
    }
}
