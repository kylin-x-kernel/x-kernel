// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Quota identities, native accounting values and operation flags.

use bitflags::bitflags;
use kerrno::{KResult, LinuxError};

/// Independent quota classes within a filesystem.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum QuotaType {
    /// Quotas indexed by filesystem user ID.
    User,
    /// Quotas indexed by filesystem group ID.
    Group,
    /// Quotas indexed by filesystem project ID.
    Project,
}

impl QuotaType {
    /// All quota classes in stable index order.
    pub const ALL: [Self; 3] = [Self::User, Self::Group, Self::Project];

    pub(crate) const fn index(self) -> usize {
        self as usize
    }

    pub(crate) const fn magic(self) -> u32 {
        match self {
            Self::User => 0xd9c0_1f11,
            Self::Group => 0xd9c0_1927,
            Self::Project => 0xd9c0_3f14,
        }
    }
}

/// A filesystem ID together with its quota class.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub struct QuotaId {
    kind: QuotaType,
    id: u32,
}

impl QuotaId {
    /// Constructs an ID already translated into the filesystem's ID domain.
    ///
    /// The all-ones ID is invalid. User-namespace mapping and authorization
    /// belong to the VFS/syscall boundary.
    pub fn new(kind: QuotaType, id: u32) -> KResult<Self> {
        if id == u32::MAX {
            return Err(LinuxError::EINVAL.into());
        }
        Ok(Self { kind, id })
    }

    /// Returns the quota class.
    pub const fn kind(self) -> QuotaType {
        self.kind
    }

    /// Returns the filesystem ID.
    pub const fn id(self) -> u32 {
        self.id
    }
}

/// Supported v2 quota file revisions and their format IDs.
#[repr(u32)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum QuotaFormat {
    /// Revision zero: 32-bit inode counts and space limits in 1 KiB units.
    VfsV0 = 2,
    /// Revision one: 64-bit counts and space limits in 1 KiB units.
    VfsV1 = 4,
}

impl QuotaFormat {
    /// Converts a quota format ID, rejecting unsupported formats.
    pub fn from_id(id: u32) -> KResult<Self> {
        match id {
            2 => Ok(Self::VfsV0),
            4 => Ok(Self::VfsV1),
            _ => Err(LinuxError::ESRCH.into()),
        }
    }

    /// Returns the format's quota-control identifier.
    pub const fn id(self) -> u32 {
        self as u32
    }

    /// Largest supported space limit in bytes.
    pub const fn max_space_limit_bytes(self) -> i64 {
        match self {
            Self::VfsV0 => (u32::MAX as i64) << 10,
            Self::VfsV1 => i64::MAX,
        }
    }

    /// Largest supported inode limit.
    pub const fn max_inode_limit(self) -> i64 {
        match self {
            Self::VfsV0 => u32::MAX as i64,
            Self::VfsV1 => i64::MAX,
        }
    }

    pub(crate) const fn version(self) -> u32 {
        match self {
            Self::VfsV0 => 0,
            Self::VfsV1 => 1,
        }
    }

    pub(crate) const fn entry_size(self) -> usize {
        match self {
            Self::VfsV0 => 48,
            Self::VfsV1 => 72,
        }
    }
}

/// Native per-ID usage, limits and absolute real-time grace deadlines.
///
/// Zero limits mean unlimited; zero deadlines mean that no grace period has
/// started. Reserved bytes are transient and are never encoded in quota files.
/// Signed usage permits administrative reconciliation while reservations exist.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Dqblk {
    /// Hard space limit in bytes.
    pub space_hard_limit_bytes: i64,
    /// Soft space limit in bytes.
    pub space_soft_limit_bytes: i64,
    /// Allocated bytes, excluding reservations.
    pub used_bytes: i64,
    /// Bytes reserved for delayed allocation.
    pub reserved_bytes: i64,
    /// Hard inode limit.
    pub inode_hard_limit: i64,
    /// Soft inode limit.
    pub inode_soft_limit: i64,
    /// Allocated inode count.
    pub used_inodes: i64,
    /// Absolute space-grace deadline in real-time seconds.
    pub space_expires_at: i64,
    /// Absolute inode-grace deadline in real-time seconds.
    pub inode_expires_at: i64,
}

impl Dqblk {
    /// Returns space visible to quota control, including delayed reservations.
    pub fn total_bytes(self) -> KResult<i64> {
        self.used_bytes
            .checked_add(self.reserved_bytes)
            .ok_or_else(crate::overflow)
    }

    pub(crate) fn has_limits(self) -> bool {
        self.space_hard_limit_bytes != 0
            || self.space_soft_limit_bytes != 0
            || self.inode_hard_limit != 0
            || self.inode_soft_limit != 0
    }

    pub(crate) fn is_unused(self) -> bool {
        !self.has_limits()
            && self.used_bytes == 0
            && self.used_inodes == 0
            && self.reserved_bytes == 0
    }
}

bitflags! {
    /// Fields modified by an administrative quota update.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct QuotaFields: u16 {
        /// Soft space limit.
        const SPACE_SOFT = 1 << 0;
        /// Hard space limit.
        const SPACE_HARD = 1 << 1;
        /// Total used space, including existing reservations.
        const SPACE_USED = 1 << 2;
        /// Soft inode limit.
        const INODE_SOFT = 1 << 3;
        /// Hard inode limit.
        const INODE_HARD = 1 << 4;
        /// Used inode count.
        const INODES = 1 << 5;
        /// Space-grace deadline.
        const SPACE_TIME = 1 << 6;
        /// Inode-grace deadline.
        const INODE_TIME = 1 << 7;
    }

    /// Admission policy for a space charge.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct ChargeFlags: u8 {
        /// Start a grace period when crossing a soft limit. Without this
        /// flag, a new soft-limit crossing is refused as preallocation.
        const WARN = 1 << 0;
        /// Charge transient reservations instead of allocated bytes.
        const RESERVE = 1 << 1;
        /// Account even when limits would refuse the charge.
        const NOFAIL = 1 << 2;
        /// Caller has authorized resource-limit bypass.
        const BYPASS_LIMITS = 1 << 3;
    }

    /// State of a loaded quota class.
    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    pub struct QuotaState: u8 {
        /// Usage accounting is loaded.
        const USAGE_ENABLED = 1 << 0;
        /// Limits are enforced as well as accounted.
        const LIMITS_ENABLED = 1 << 1;
        /// Loaded quota state is suspended.
        const SUSPENDED = 1 << 2;
    }
}
