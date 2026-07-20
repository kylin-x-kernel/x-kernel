// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{Ext4Error, Ext4Result, FeatureClass};

bitflags::bitflags! {
    /// Compatible ext4 superblock feature flags.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct CompatFeatures: u32 {
        /// The filesystem has an internal or external journal.
        const HAS_JOURNAL = 0x0004;
        /// Directories may use an HTree index.
        const DIR_INDEX = 0x0020;
        /// Backup superblocks use the sparse-super2 layout.
        const SPARSE_SUPER2 = 0x0200;
        /// Orphans are tracked in the orphan-file format.
        const ORPHAN_FILE = 0x1000;
    }

    /// Incompatible ext4 superblock feature flags.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct IncompatFeatures: u32 {
        /// Directory entries carry their file type.
        const FILETYPE = 0x0002;
        /// The journal must be replayed before normal access.
        const RECOVER = 0x0004;
        /// Inodes use extent trees for block mapping.
        const EXTENTS = 0x0040;
        /// Superblock and group descriptors carry 64-bit block numbers.
        const BIT_64 = 0x0080;
        /// Extended attributes may be stored in dedicated inodes.
        const EA_INODE = 0x0400;
        /// Block groups are organized into flex groups.
        const FLEX_BG = 0x0200;
        /// Metadata checksums use the stored checksum seed.
        const CSUM_SEED = 0x2000;
    }

    /// Read-only-compatible ext4 superblock feature flags.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ReadOnlyCompatFeatures: u32 {
        /// Backup superblocks use the sparse-super layout.
        const SPARSE_SUPER = 0x0001;
        /// Inodes may use huge-file block accounting.
        const HUGE_FILE = 0x0008;
        /// Group descriptors carry legacy CRC16 checksums.
        const GDT_CSUM = 0x0010;
        /// Allocation uses clusters larger than filesystem blocks.
        const BIGALLOC = 0x0200;
        /// Metadata structures carry CRC32C checksums.
        const METADATA_CSUM = 0x0400;
        /// Directories may exceed the legacy htree size limits.
        const LARGE_DIR = 0x4000;
        /// The orphan file contains entries pending cleanup.
        const ORPHAN_PRESENT = 0x0001_0000;
    }
}

const SUPPORTED_INCOMPAT: IncompatFeatures = IncompatFeatures::FILETYPE
    .union(IncompatFeatures::EXTENTS)
    .union(IncompatFeatures::BIT_64)
    .union(IncompatFeatures::EA_INODE)
    .union(IncompatFeatures::FLEX_BG)
    .union(IncompatFeatures::CSUM_SEED);

/// Ext4 feature flags decoded and negotiated during mount.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureSet {
    compat: CompatFeatures,
    incompat: IncompatFeatures,
    read_only_compat: ReadOnlyCompatFeatures,
}

impl FeatureSet {
    pub(crate) const fn new(compat: u32, incompat: u32, read_only_compat: u32) -> Self {
        Self {
            compat: CompatFeatures::from_bits_retain(compat),
            incompat: IncompatFeatures::from_bits_retain(incompat),
            read_only_compat: ReadOnlyCompatFeatures::from_bits_retain(read_only_compat),
        }
    }

    /// Returns the compatible feature flags, including unknown on-disk bits.
    pub const fn compat(self) -> CompatFeatures {
        self.compat
    }

    /// Returns the incompatible feature flags, including unknown on-disk bits.
    pub const fn incompat(self) -> IncompatFeatures {
        self.incompat
    }

    /// Returns the read-only-compatible feature flags, including unknown on-disk bits.
    pub const fn read_only_compat(self) -> ReadOnlyCompatFeatures {
        self.read_only_compat
    }

    /// Returns whether the filesystem has an internal or external journal.
    pub const fn has_journal(self) -> bool {
        self.compat.contains(CompatFeatures::HAS_JOURNAL)
    }

    pub(crate) const fn has_dir_index(self) -> bool {
        self.compat.contains(CompatFeatures::DIR_INDEX)
    }

    /// Returns whether the journal must be replayed before normal access.
    pub const fn needs_recovery(self) -> bool {
        self.incompat.contains(IncompatFeatures::RECOVER)
    }

    /// Returns whether metadata checksums are enabled.
    pub const fn has_metadata_checksum(self) -> bool {
        self.read_only_compat
            .contains(ReadOnlyCompatFeatures::METADATA_CSUM)
    }

    pub(crate) const fn has_huge_file(self) -> bool {
        self.read_only_compat
            .contains(ReadOnlyCompatFeatures::HUGE_FILE)
    }

    pub(crate) const fn has_sparse_super(self) -> bool {
        self.read_only_compat
            .contains(ReadOnlyCompatFeatures::SPARSE_SUPER)
    }

    pub(crate) const fn has_sparse_super2(self) -> bool {
        self.compat.contains(CompatFeatures::SPARSE_SUPER2)
    }

    pub(crate) const fn has_orphan_file(self) -> bool {
        self.compat.contains(CompatFeatures::ORPHAN_FILE)
    }

    pub(crate) const fn has_64bit(self) -> bool {
        self.incompat.contains(IncompatFeatures::BIT_64)
    }

    pub(crate) const fn has_extents(self) -> bool {
        self.incompat.contains(IncompatFeatures::EXTENTS)
    }

    pub(crate) const fn has_ea_inode(self) -> bool {
        self.incompat.contains(IncompatFeatures::EA_INODE)
    }

    pub(crate) const fn has_flex_bg(self) -> bool {
        self.incompat.contains(IncompatFeatures::FLEX_BG)
    }

    pub(crate) const fn has_checksum_seed(self) -> bool {
        self.incompat.contains(IncompatFeatures::CSUM_SEED)
    }

    pub(crate) const fn has_large_dir(self) -> bool {
        self.read_only_compat
            .contains(ReadOnlyCompatFeatures::LARGE_DIR)
    }

    pub(crate) const fn has_orphan_present(self) -> bool {
        self.read_only_compat
            .contains(ReadOnlyCompatFeatures::ORPHAN_PRESENT)
    }

    pub(crate) fn validate_read_only(self) -> Ext4Result<()> {
        let supported_incompat = SUPPORTED_INCOMPAT.union(IncompatFeatures::RECOVER);
        let unsupported_incompat = self.incompat.bits() & !supported_incompat.bits();
        if unsupported_incompat != 0 {
            return Err(Ext4Error::UnsupportedFeature {
                class: FeatureClass::Incompatible,
                bits: unsupported_incompat,
            });
        }

        if self
            .read_only_compat
            .contains(ReadOnlyCompatFeatures::BIGALLOC)
        {
            return Err(Ext4Error::UnsupportedFeature {
                class: FeatureClass::ReadOnlyCompatible,
                bits: ReadOnlyCompatFeatures::BIGALLOC.bits(),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::{CompatFeatures, FeatureSet, IncompatFeatures};
    use crate::{Ext4Error, FeatureClass};

    #[test]
    fn overlapping_feature_bits_remain_in_their_own_classes() {
        let features = FeatureSet::new(
            CompatFeatures::HAS_JOURNAL.bits(),
            IncompatFeatures::RECOVER.bits(),
            0,
        );

        assert!(features.has_journal());
        assert!(features.needs_recovery());
        assert_eq!(features.compat(), CompatFeatures::HAS_JOURNAL);
        assert_eq!(features.incompat(), IncompatFeatures::RECOVER);
    }

    #[test]
    fn unknown_incompatible_bits_are_retained_and_rejected() {
        const UNKNOWN: u32 = 1 << 31;
        let features = FeatureSet::new(0, UNKNOWN, 0);

        assert_eq!(features.incompat().bits(), UNKNOWN);
        assert_eq!(
            features.validate_read_only(),
            Err(Ext4Error::UnsupportedFeature {
                class: FeatureClass::Incompatible,
                bits: UNKNOWN,
            })
        );
    }
}
