// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! KExt4 adapter utilities.

use core::time::Duration;

use kerrno::LinuxError;
use kext4::{DirectoryFileType, Ext4DeviceId, Ext4Error, Ext4Timestamp, InodeKind};
use kvfs::{DeviceId, NodeType, VfsError};

pub(crate) fn into_vfs_err(err: Ext4Error) -> VfsError {
    let linux_error = match err {
        Ext4Error::NoSpace => LinuxError::ENOSPC,
        Ext4Error::AlreadyExists => LinuxError::EEXIST,
        Ext4Error::NotFound => LinuxError::ENOENT,
        Ext4Error::DirectoryNotEmpty => LinuxError::ENOTEMPTY,
        Ext4Error::InvalidName
        | Ext4Error::InvalidBufferLength { .. }
        | Ext4Error::InvalidDeviceBlockSize(_)
        | Ext4Error::OutOfBounds
        | Ext4Error::InvalidDirectoryPosition
        | Ext4Error::InvalidMagic(_)
        | Ext4Error::UnsupportedRevision(_) => LinuxError::EINVAL,
        Ext4Error::JournalBusy => LinuxError::EBUSY,
        Ext4Error::UnsupportedFeature { .. }
        | Ext4Error::UnsupportedJournalFeature { .. }
        | Ext4Error::Unsupported(_) => LinuxError::EOPNOTSUPP,
        Ext4Error::Device(_)
        | Ext4Error::JournalAborted
        | Ext4Error::InsufficientJournalCredits
        | Ext4Error::InvalidJournalTransaction
        | Ext4Error::Overflow
        | Ext4Error::NeedsRecovery
        | Ext4Error::ChecksumMismatch { .. }
        | Ext4Error::Corrupt(_) => LinuxError::EIO,
    };
    VfsError::from(linux_error).canonicalize()
}

pub(crate) const fn inode_kind_to_vfs(kind: InodeKind) -> NodeType {
    match kind {
        InodeKind::Fifo => NodeType::Fifo,
        InodeKind::CharacterDevice => NodeType::CharacterDevice,
        InodeKind::Directory => NodeType::Directory,
        InodeKind::BlockDevice => NodeType::BlockDevice,
        InodeKind::RegularFile => NodeType::RegularFile,
        InodeKind::Symlink => NodeType::Symlink,
        InodeKind::Socket => NodeType::Socket,
    }
}

pub(crate) const fn vfs_type_to_inode_kind(node_type: NodeType) -> Option<InodeKind> {
    match node_type {
        NodeType::Fifo => Some(InodeKind::Fifo),
        NodeType::CharacterDevice => Some(InodeKind::CharacterDevice),
        NodeType::Directory => Some(InodeKind::Directory),
        NodeType::BlockDevice => Some(InodeKind::BlockDevice),
        NodeType::RegularFile => Some(InodeKind::RegularFile),
        NodeType::Symlink => Some(InodeKind::Symlink),
        NodeType::Socket => Some(InodeKind::Socket),
        NodeType::Unknown => None,
    }
}

pub(crate) const fn dir_entry_type_to_vfs(file_type: DirectoryFileType) -> NodeType {
    match file_type {
        DirectoryFileType::RegularFile => NodeType::RegularFile,
        DirectoryFileType::Directory => NodeType::Directory,
        DirectoryFileType::CharacterDevice => NodeType::CharacterDevice,
        DirectoryFileType::BlockDevice => NodeType::BlockDevice,
        DirectoryFileType::Fifo => NodeType::Fifo,
        DirectoryFileType::Socket => NodeType::Socket,
        DirectoryFileType::Symlink => NodeType::Symlink,
        DirectoryFileType::Unknown => NodeType::Unknown,
    }
}

pub(crate) const fn device_id_to_ext4(device: DeviceId) -> Ext4DeviceId {
    Ext4DeviceId::new(device.major(), device.minor())
}

pub(crate) const fn ext4_device_id_to_vfs(device: Ext4DeviceId) -> DeviceId {
    DeviceId::new(device.major(), device.minor())
}

pub(crate) fn duration_to_ext4(timestamp: Duration) -> Ext4Timestamp {
    Ext4Timestamp::new(timestamp.as_secs() as i64, timestamp.subsec_nanos())
}

pub(crate) fn ext4_timestamp_to_duration(timestamp: Ext4Timestamp) -> Duration {
    if timestamp.seconds() <= 0 {
        Duration::ZERO
    } else {
        Duration::new(timestamp.seconds() as u64, timestamp.nanos())
    }
}

#[cfg(feature = "times")]
pub(crate) fn current_ext4_timestamp() -> Ext4Timestamp {
    duration_to_ext4(khal::time::wall_time())
}

#[cfg(not(feature = "times"))]
pub(crate) fn current_ext4_timestamp() -> Ext4Timestamp {
    Ext4Timestamp::new(0, 0)
}
