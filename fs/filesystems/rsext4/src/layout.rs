// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Derived layout and feature capability layer.
//!
//! These types are built from the on-disk superblock once at mount time and
//! cache commonly-accessed values so callers do not need to read raw
//! superblock fields.  They follow the same caching pattern as Linux's
//! `ext4_sb_info` (see `s_blocks_per_group`, `s_inodes_per_group`,
//! `s_itb_per_group`, `s_log_groups_per_flex` in `ext4.h`).

use crate::{config::*, error::RSEXT4Error, superblock::Ext4Superblock};

const EXT4_GOOD_OLD_INODE_SIZE: u16 = 128;
const EXT4_GOOD_OLD_FIRST_INO: u32 = 11;
const EXT4_MIN_DESC_SIZE_64BIT: u16 = 64;
const EXT4_MAX_DESC_SIZE: u16 = 1024;

/// Normalised feature flags derived from the on-disk superblock.
///
/// `log_groups_per_flex` mirrors Linux `sbi->s_log_groups_per_flex`
/// (`ext4.h:1671`).  When 0, flex-group operations are inactive.
#[derive(Debug, Clone, Copy)]
pub struct Ext4Features {
    /// Runtime gate, matching Linux `sbi->s_log_groups_per_flex != 0`.
    /// True only when `EXT4_FEATURE_INCOMPAT_FLEX_BG` is set AND
    /// `s_log_groups_per_flex` is in the valid range 1..=31 (Linux
    /// `super.c:3218`).
    has_flex_bg: bool,
    /// Mirrors Linux `sbi->s_log_groups_per_flex`.  0 means flex
    /// operations are inactive.
    log_groups_per_flex: u32,
}

/// Cached layout values and derived feature set built from the on-disk
/// superblock at mount time.  These mirror the cached values in Linux's
/// `ext4_sb_info`.
#[derive(Debug, Clone)]
pub struct Ext4Layout {
    pub(crate) block_size: u64,
    pub(crate) blocks_per_group: u32,
    pub(crate) inodes_per_group: u32,
    pub(crate) group_count: u32,
    pub(crate) inode_size: u16,
    pub(crate) first_inode: u32,
    pub(crate) inode_table_blocks_per_group: u32,
    pub(crate) desc_size: u16,
    pub(crate) descs_per_block: u32,
    pub(crate) first_data_block: u32,
    features: Ext4Features,
}

impl Ext4Features {
    /// Derive normalised feature flags from the on-disk superblock.
    ///
    /// # Linux reference
    ///
    /// `ext4_fill_flex_info()` (`super.c:3209-3241`) clamps invalid
    /// `s_log_groups_per_flex` to 0 and returns early, leaving flex
    /// operations inactive.  We mirror that here by setting
    /// `has_flex_bg = false` when the log value is out of range.
    pub fn from_superblock(sb: &Ext4Superblock) -> Self {
        // Linux super.c:3218: clamp to 0 when < 1 or > 31
        let (has_flex_bg, log_groups_per_flex) =
            if sb.has_flex_bg() && (1..=31).contains(&sb.s_log_groups_per_flex) {
                (true, sb.s_log_groups_per_flex as u32)
            } else {
                (false, 0)
            };

        Self {
            has_flex_bg,
            log_groups_per_flex,
        }
    }
}

impl Ext4Layout {
    /// Builds cached layout from the on-disk superblock.
    pub fn try_from_superblock(sb: &Ext4Superblock) -> Result<Self, RSEXT4Error> {
        Self::validate_log_block_size(sb)?;

        let block_size = sb.block_size();
        if block_size != BLOCK_SIZE as u64 {
            return Err(RSEXT4Error::InvalidSuperblock);
        }

        let features = Ext4Features::from_superblock(sb);
        let inode_size = Self::inode_size_from_superblock(sb, block_size)?;
        let first_inode = Self::first_inode_from_superblock(sb)?;
        let desc_size = Self::desc_size_from_superblock(sb)?;
        let block_bitmap_bits = (block_size as u32).saturating_mul(8);
        let inodes_per_block = (block_size / inode_size as u64) as u32;
        if inodes_per_block == 0 || sb.s_blocks_per_group == 0 {
            return Err(RSEXT4Error::InvalidSuperblock);
        }
        if sb.s_blocks_per_group > block_bitmap_bits
            || sb.s_clusters_per_group > block_bitmap_bits
            || sb.s_blocks_per_group != sb.s_clusters_per_group
        {
            return Err(RSEXT4Error::InvalidSuperblock);
        }
        if sb.s_inodes_per_group < inodes_per_block || sb.s_inodes_per_group > block_bitmap_bits {
            return Err(RSEXT4Error::InvalidSuperblock);
        }

        let inode_table_blocks_per_group =
            (sb.s_inodes_per_group as u64 * inode_size as u64).div_ceil(block_size) as u32;
        let descs_per_block = block_size as u32 / desc_size as u32;
        let group_count = sb.block_groups_count();
        if group_count == 0 {
            return Err(RSEXT4Error::InvalidSuperblock);
        }

        Ok(Self {
            block_size,
            blocks_per_group: sb.s_blocks_per_group,
            inodes_per_group: sb.s_inodes_per_group,
            group_count,
            inode_size,
            first_inode,
            inode_table_blocks_per_group,
            desc_size,
            descs_per_block,
            first_data_block: sb.s_first_data_block,
            features,
        })
    }

    fn validate_log_block_size(sb: &Ext4Superblock) -> Result<(), RSEXT4Error> {
        if sb.s_log_block_size != LOG_BLOCK_SIZE || sb.s_log_cluster_size != LOG_BLOCK_SIZE {
            return Err(RSEXT4Error::InvalidSuperblock);
        }
        Ok(())
    }

    fn inode_size_from_superblock(
        sb: &Ext4Superblock,
        block_size: u64,
    ) -> Result<u16, RSEXT4Error> {
        let inode_size = if sb.s_rev_level == Ext4Superblock::EXT4_GOOD_OLD_REV {
            EXT4_GOOD_OLD_INODE_SIZE
        } else {
            sb.s_inode_size
        };

        if inode_size < EXT4_GOOD_OLD_INODE_SIZE
            || !inode_size.is_power_of_two()
            || inode_size as u64 > block_size
        {
            return Err(RSEXT4Error::InvalidSuperblock);
        }

        Ok(inode_size)
    }

    fn first_inode_from_superblock(sb: &Ext4Superblock) -> Result<u32, RSEXT4Error> {
        let first_inode = if sb.s_rev_level == Ext4Superblock::EXT4_GOOD_OLD_REV {
            EXT4_GOOD_OLD_FIRST_INO
        } else {
            sb.s_first_ino
        };

        if first_inode < EXT4_GOOD_OLD_FIRST_INO {
            return Err(RSEXT4Error::InvalidSuperblock);
        }

        Ok(first_inode)
    }

    fn desc_size_from_superblock(sb: &Ext4Superblock) -> Result<u16, RSEXT4Error> {
        if !sb.has_feature_incompat(Ext4Superblock::EXT4_FEATURE_INCOMPAT_64BIT) {
            return Ok(GROUP_DESC_SIZE_OLD);
        }

        let desc_size = sb.s_desc_size;
        if !(EXT4_MIN_DESC_SIZE_64BIT..=EXT4_MAX_DESC_SIZE).contains(&desc_size)
            || !desc_size.is_power_of_two()
        {
            return Err(RSEXT4Error::InvalidSuperblock);
        }

        Ok(desc_size)
    }

    /// Returns the flex group index for a given block group.
    ///
    /// Matches Linux `ext4_flex_group()` (`ext4.h:3443`):
    /// `block_group >> sbi->s_log_groups_per_flex`.
    ///
    /// When flex is inactive (`log_groups_per_flex == 0`), returns
    /// `block_group` — each group is its own flex group.
    pub fn flex_group_of(&self, block_group: u32) -> u32 {
        block_group >> self.features.log_groups_per_flex
    }

    /// Returns the number of block groups in a flex group.
    ///
    /// Matches Linux `ext4_flex_bg_size()` (`ext4.h:3449`):
    /// `1 << sbi->s_log_groups_per_flex`.
    pub fn flex_bg_size(&self) -> u32 {
        1u32 << self.features.log_groups_per_flex
    }

    /// Returns `true` if flex_bg operations are active.
    ///
    /// Matches the Linux runtime gate `sbi->s_log_groups_per_flex != 0`
    /// used at `ialloc.c:336`, `mballoc.c:4163`, `super.c:3168`.
    pub fn has_flex_bg(&self) -> bool {
        self.features.has_flex_bg
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_superblock() -> Ext4Superblock {
        let mut sb = Ext4Superblock::default();
        sb.s_rev_level = Ext4Superblock::EXT4_DYNAMIC_REV;
        sb.s_blocks_per_group = 8192;
        sb.s_clusters_per_group = 8192;
        sb.s_inodes_per_group = 256;
        sb.s_inode_size = 256;
        sb.s_first_ino = EXT4_GOOD_OLD_FIRST_INO;
        sb.s_blocks_count_lo = 32768;
        sb.s_log_block_size = 2;
        sb.s_log_cluster_size = 2;
        sb.s_first_data_block = 0;
        sb.s_desc_size = 64;
        sb.s_feature_incompat = Ext4Superblock::SUPPORTED_INCOMPAT_FEATURES;
        sb
    }

    #[test]
    fn derived_layout_matches_superblock() {
        let sb = make_superblock();
        let layout = Ext4Layout::try_from_superblock(&sb).unwrap();

        assert_eq!(layout.block_size, 4096);
        assert_eq!(layout.blocks_per_group, 8192);
        assert_eq!(layout.inodes_per_group, 256);
        assert_eq!(layout.inode_size, 256);
        assert_eq!(layout.first_data_block, 0);
        assert_eq!(layout.group_count, 4);
        assert_eq!(layout.inode_table_blocks_per_group, 16);
        assert_eq!(layout.descs_per_block, 64);
    }

    #[test]
    fn rejects_non_4k_block_size() {
        let mut sb = make_superblock();
        sb.s_log_block_size = 0;

        assert!(Ext4Layout::try_from_superblock(&sb).is_err());
    }

    #[test]
    fn rejects_invalid_inode_size() {
        let mut sb = make_superblock();
        sb.s_inode_size = 192;

        assert!(Ext4Layout::try_from_superblock(&sb).is_err());
    }

    #[test]
    fn rejects_invalid_64bit_descriptor_size() {
        let mut sb = make_superblock();
        sb.s_desc_size = 48;

        assert!(Ext4Layout::try_from_superblock(&sb).is_err());
    }

    #[test]
    fn rejects_blocks_per_group_larger_than_block_bitmap() {
        let mut sb = make_superblock();
        sb.s_blocks_per_group = 32769;
        sb.s_clusters_per_group = 32769;

        assert!(Ext4Layout::try_from_superblock(&sb).is_err());
    }

    #[test]
    fn rejects_cluster_geometry_that_differs_from_block_geometry() {
        let mut sb = make_superblock();
        sb.s_clusters_per_group = sb.s_blocks_per_group - 1;

        assert!(Ext4Layout::try_from_superblock(&sb).is_err());
    }

    #[test]
    fn rejects_invalid_dynamic_first_inode() {
        let mut sb = make_superblock();
        sb.s_first_ino = EXT4_GOOD_OLD_FIRST_INO - 1;

        assert!(Ext4Layout::try_from_superblock(&sb).is_err());
    }

    #[test]
    fn no_flex_bg_each_group_is_own_flex_group() {
        let sb = make_superblock();
        let layout = Ext4Layout::try_from_superblock(&sb).unwrap();

        assert!(!layout.has_flex_bg());
        assert_eq!(layout.features.log_groups_per_flex, 0);
        assert_eq!(layout.flex_bg_size(), 1); // 1 << 0
        // Linux: flex_group_of(n) = n >> 0 = n
        assert_eq!(layout.flex_group_of(0), 0);
        assert_eq!(layout.flex_group_of(5), 5);
        assert_eq!(layout.flex_group_of(100), 100);
    }

    #[test]
    fn flex_bg_with_valid_log_groups_per_flex() {
        let mut sb = make_superblock();
        sb.s_feature_incompat |= Ext4Superblock::EXT4_FEATURE_INCOMPAT_FLEX_BG;
        sb.s_log_groups_per_flex = 4;

        let layout = Ext4Layout::try_from_superblock(&sb).unwrap();

        assert!(layout.has_flex_bg());
        assert_eq!(layout.features.log_groups_per_flex, 4);
        assert_eq!(layout.flex_bg_size(), 16); // 1 << 4
        assert_eq!(layout.flex_group_of(0), 0);
        assert_eq!(layout.flex_group_of(15), 0);
        assert_eq!(layout.flex_group_of(16), 1);
        assert_eq!(layout.flex_group_of(31), 1);
    }

    #[test]
    fn flex_bg_with_out_of_range_log_is_inactive() {
        let mut sb = make_superblock();
        sb.s_feature_incompat |= Ext4Superblock::EXT4_FEATURE_INCOMPAT_FLEX_BG;
        sb.s_log_groups_per_flex = 0; // invalid — Linux clamps

        let layout = Ext4Layout::try_from_superblock(&sb).unwrap();

        // Linux super.c:3218-3220: clamps log to 0, flex is inactive
        assert!(!layout.has_flex_bg());
        assert_eq!(layout.features.log_groups_per_flex, 0);
        assert_eq!(layout.flex_bg_size(), 1);
        // Each group is its own flex group
        assert_eq!(layout.flex_group_of(5), 5);
    }

    #[test]
    fn flex_bg_with_log_above_31_is_inactive() {
        let mut sb = make_superblock();
        sb.s_feature_incompat |= Ext4Superblock::EXT4_FEATURE_INCOMPAT_FLEX_BG;
        sb.s_log_groups_per_flex = 32; // > 31 — Linux super.c:3218 clamps

        let layout = Ext4Layout::try_from_superblock(&sb).unwrap();

        assert!(!layout.has_flex_bg());
        assert_eq!(layout.features.log_groups_per_flex, 0);
        assert_eq!(layout.flex_bg_size(), 1);
        assert_eq!(layout.flex_group_of(5), 5);
    }

    #[test]
    fn flex_bg_with_log_max_u8_is_inactive() {
        let mut sb = make_superblock();
        sb.s_feature_incompat |= Ext4Superblock::EXT4_FEATURE_INCOMPAT_FLEX_BG;
        sb.s_log_groups_per_flex = 255; // u8 max — Linux super.c:3218 clamps

        let layout = Ext4Layout::try_from_superblock(&sb).unwrap();

        assert!(!layout.has_flex_bg());
        assert_eq!(layout.features.log_groups_per_flex, 0);
    }

    #[test]
    fn has_flex_bg_is_false_when_flag_not_set() {
        let sb = make_superblock();
        let features = Ext4Features::from_superblock(&sb);
        assert!(!features.has_flex_bg);
        assert_eq!(features.log_groups_per_flex, 0);
    }
}
