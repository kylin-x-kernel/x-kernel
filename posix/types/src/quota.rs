// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Disk quota syscall ABI. Limits use 1 KiB units and usage uses bytes.

use crate::{UserRead, UserWrite};

/// Classic quota record, including explicitly initialized trailing ABI padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct IfDqblk {
    /// Absolute space limit, in 1-KiB blocks.
    pub block_hard_limit: u64,
    /// Space limit enforced after the grace deadline, in 1-KiB blocks.
    pub block_soft_limit: u64,
    /// Allocated and reserved space, in bytes.
    pub current_space: u64,
    /// Absolute inode-count limit.
    pub inode_hard_limit: u64,
    /// Inode-count limit enforced after the grace deadline.
    pub inode_soft_limit: u64,
    /// Current charged inode count.
    pub current_inodes: u64,
    /// Space grace expiration, in seconds since the Unix epoch.
    pub block_time: u64,
    /// Inode grace expiration, in seconds since the Unix epoch.
    pub inode_time: u64,
    /// Bitmask identifying meaningful fields.
    pub valid: u32,
    /// Explicit trailing alignment bytes, zero on output.
    pub abi_padding: u32,
}

/// Quota enumeration record. The ID occupies the classic record's tail padding.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct IfNextDqblk {
    /// Absolute space limit, in 1-KiB blocks.
    pub block_hard_limit: u64,
    /// Space limit enforced after the grace deadline, in 1-KiB blocks.
    pub block_soft_limit: u64,
    /// Allocated and reserved space, in bytes.
    pub current_space: u64,
    /// Absolute inode-count limit.
    pub inode_hard_limit: u64,
    /// Inode-count limit enforced after the grace deadline.
    pub inode_soft_limit: u64,
    /// Current charged inode count.
    pub current_inodes: u64,
    /// Space grace expiration, in seconds since the Unix epoch.
    pub block_time: u64,
    /// Inode grace expiration, in seconds since the Unix epoch.
    pub inode_time: u64,
    /// Bitmask identifying meaningful fields.
    pub valid: u32,
    /// User, group, or project ID selected by the command.
    pub id: u32,
}

/// Grace durations and quota-file properties.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct IfDqinfo {
    /// Space grace duration, in seconds.
    pub block_grace: u64,
    /// Inode grace duration, in seconds.
    pub inode_grace: u64,
    /// Quota class or quota-state flags, as defined by this carrier.
    pub flags: u32,
    /// Bitmask identifying meaningful fields.
    pub valid: u32,
}

const _: () = {
    assert!(core::mem::size_of::<IfDqblk>() == 72);
    assert!(core::mem::size_of::<IfNextDqblk>() == 72);
    assert!(core::mem::size_of::<IfDqinfo>() == 24);
};

/// Extended quota record. Space counters use 512-byte basic blocks.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct FsDiskQuota {
    /// Requested or returned structure version.
    pub version: i8,
    /// Quota class or quota-state flags, as defined by this carrier.
    pub flags: i8,
    /// Selected extended record fields and timestamp representation.
    pub fieldmask: u16,
    /// User, group, or project ID selected by the command.
    pub id: u32,
    /// Absolute space limit, in 512-byte blocks.
    pub block_hard_limit: u64,
    /// Space limit enforced after the grace deadline, in 512-byte blocks.
    pub block_soft_limit: u64,
    /// Absolute inode-count limit.
    pub inode_hard_limit: u64,
    /// Inode-count limit enforced after the grace deadline.
    pub inode_soft_limit: u64,
    /// Current allocated and reserved space, in 512-byte blocks.
    pub block_count: u64,
    /// Current charged inode count.
    pub inode_count: u64,
    /// Low 32 bits of the inode grace expiration.
    pub inode_timer: i32,
    /// Low 32 bits of the space grace expiration.
    pub block_timer: i32,
    /// Inode warning count or threshold.
    pub inode_warnings: u16,
    /// Space warning count or threshold.
    pub block_warnings: u16,
    /// Signed upper eight bits of an extended inode expiration.
    pub inode_timer_hi: i8,
    /// Signed upper eight bits of an extended space expiration.
    pub block_timer_hi: i8,
    /// Signed upper eight bits of the realtime grace expiration.
    pub realtime_timer_hi: i8,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding2: i8,
    /// Realtime space hard limit, in 512-byte blocks.
    pub realtime_hard_limit: u64,
    /// Realtime space soft limit, in 512-byte blocks.
    pub realtime_soft_limit: u64,
    /// Realtime usage, in 512-byte blocks.
    pub realtime_count: u64,
    /// Low 32 bits of the realtime grace expiration.
    pub realtime_timer: i32,
    /// Realtime warning count or threshold.
    pub realtime_warnings: u16,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding3: i16,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding4: [u8; 8],
}

/// Backing-file usage in quota status responses.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct FsQFileStat {
    /// Backing-file inode number, zero when absent.
    pub inode: u64,
    /// Backing-file allocation, in 512-byte blocks.
    pub blocks: u64,
    /// Reported backing-file extent count.
    pub extents: u32,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding: u32,
}

/// Original quota status ABI, with all alignment bytes explicitly represented.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct FsQuotaStat {
    /// Requested or returned structure version.
    pub version: i8,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding1: u8,
    /// Quota class or quota-state flags, as defined by this carrier.
    pub flags: u16,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding2: u32,
    /// User quota backing-file statistics.
    pub user: FsQFileStat,
    /// Group quota backing-file statistics.
    pub group: FsQFileStat,
    /// Number of resident cached quota records.
    pub in_core_records: u32,
    /// Space grace duration, in seconds.
    pub block_grace: i32,
    /// Inode grace duration, in seconds.
    pub inode_grace: i32,
    /// Realtime grace duration, in seconds.
    pub realtime_grace: i32,
    /// Space warning count or threshold.
    pub block_warnings: u16,
    /// Inode warning count or threshold.
    pub inode_warnings: u16,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding3: u32,
}

/// Versioned quota status ABI, including project-file statistics.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserRead, UserWrite)]
pub struct FsQuotaStatV {
    /// Requested or returned structure version.
    pub version: i8,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding1: u8,
    /// Quota class or quota-state flags, as defined by this carrier.
    pub flags: u16,
    /// Number of resident cached quota records.
    pub in_core_records: u32,
    /// User quota backing-file statistics.
    pub user: FsQFileStat,
    /// Group quota backing-file statistics.
    pub group: FsQFileStat,
    /// Project quota backing-file statistics.
    pub project: FsQFileStat,
    /// Space grace duration, in seconds.
    pub block_grace: i32,
    /// Inode grace duration, in seconds.
    pub inode_grace: i32,
    /// Realtime grace duration, in seconds.
    pub realtime_grace: i32,
    /// Space warning count or threshold.
    pub block_warnings: u16,
    /// Inode warning count or threshold.
    pub inode_warnings: u16,
    /// Realtime warning count or threshold.
    pub realtime_warnings: u16,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding3: u16,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding4: u32,
    /// Reserved alignment or extension bytes, zero on output.
    pub padding2: [u64; 7],
}

const _: () = {
    assert!(core::mem::size_of::<FsDiskQuota>() == 112);
    assert!(core::mem::size_of::<FsQFileStat>() == 24);
    assert!(core::mem::size_of::<FsQuotaStat>() == 80);
    assert!(core::mem::size_of::<FsQuotaStatV>() == 160);
};
