// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use crate::{Ext4Error, Ext4Result, FeatureClass};

pub(crate) const COMPAT_HAS_JOURNAL: u32 = 0x0004;

pub(crate) const INCOMPAT_FILETYPE: u32 = 0x0002;
pub(crate) const INCOMPAT_RECOVER: u32 = 0x0004;
pub(crate) const INCOMPAT_EXTENTS: u32 = 0x0040;
pub(crate) const INCOMPAT_64BIT: u32 = 0x0080;
pub(crate) const INCOMPAT_FLEX_BG: u32 = 0x0200;
pub(crate) const INCOMPAT_CSUM_SEED: u32 = 0x2000;

pub(crate) const RO_COMPAT_GDT_CSUM: u32 = 0x0010;
pub(crate) const RO_COMPAT_BIGALLOC: u32 = 0x0200;
pub(crate) const RO_COMPAT_METADATA_CSUM: u32 = 0x0400;

const SUPPORTED_INCOMPAT: u32 =
    INCOMPAT_FILETYPE | INCOMPAT_EXTENTS | INCOMPAT_64BIT | INCOMPAT_FLEX_BG | INCOMPAT_CSUM_SEED;

/// Raw ext4 feature bitmaps negotiated during mount.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct FeatureSet {
    compat: u32,
    incompat: u32,
    read_only_compat: u32,
}

impl FeatureSet {
    pub(crate) const fn new(compat: u32, incompat: u32, read_only_compat: u32) -> Self {
        Self {
            compat,
            incompat,
            read_only_compat,
        }
    }

    /// Returns the compatible feature bitmap.
    pub const fn compat(self) -> u32 {
        self.compat
    }

    /// Returns the incompatible feature bitmap.
    pub const fn incompat(self) -> u32 {
        self.incompat
    }

    /// Returns the read-only-compatible feature bitmap.
    pub const fn read_only_compat(self) -> u32 {
        self.read_only_compat
    }

    /// Returns whether the filesystem has an internal or external journal.
    pub const fn has_journal(self) -> bool {
        self.compat & COMPAT_HAS_JOURNAL != 0
    }

    /// Returns whether metadata checksums are enabled.
    pub const fn has_metadata_checksum(self) -> bool {
        self.read_only_compat & RO_COMPAT_METADATA_CSUM != 0
    }

    pub(crate) const fn has_64bit(self) -> bool {
        self.incompat & INCOMPAT_64BIT != 0
    }

    pub(crate) const fn has_flex_bg(self) -> bool {
        self.incompat & INCOMPAT_FLEX_BG != 0
    }

    pub(crate) const fn has_checksum_seed(self) -> bool {
        self.incompat & INCOMPAT_CSUM_SEED != 0
    }

    pub(crate) fn validate_read_only(self) -> Ext4Result<()> {
        if self.incompat & INCOMPAT_RECOVER != 0 {
            return Err(Ext4Error::NeedsRecovery);
        }

        let unsupported_incompat = self.incompat & !(SUPPORTED_INCOMPAT | INCOMPAT_RECOVER);
        if unsupported_incompat != 0 {
            return Err(Ext4Error::UnsupportedFeature {
                class: FeatureClass::Incompatible,
                bits: unsupported_incompat,
            });
        }

        if self.read_only_compat & RO_COMPAT_BIGALLOC != 0 {
            return Err(Ext4Error::UnsupportedFeature {
                class: FeatureClass::ReadOnlyCompatible,
                bits: RO_COMPAT_BIGALLOC,
            });
        }

        if self.incompat & INCOMPAT_EXTENTS == 0 {
            return Err(Ext4Error::UnsupportedFeature {
                class: FeatureClass::Incompatible,
                bits: INCOMPAT_EXTENTS,
            });
        }

        Ok(())
    }
}
