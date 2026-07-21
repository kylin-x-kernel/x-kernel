// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common VFS data types.
use core::{fmt::Debug, time::Duration};

/// Filesystem node type values.
#[repr(u8)]
#[derive(Debug, Clone, Copy, Eq, PartialEq)]
pub enum NodeType {
    Unknown         = 0,
    Fifo            = 0o1,
    CharacterDevice = 0o2,
    Directory       = 0o4,
    BlockDevice     = 0o6,
    RegularFile     = 0o10,
    Symlink         = 0o12,
    Socket          = 0o14,
}

impl NodeType {
    const fn from_mode_type(value: u8) -> Self {
        match value {
            0o1 => Self::Fifo,
            0o2 => Self::CharacterDevice,
            0o4 => Self::Directory,
            0o6 => Self::BlockDevice,
            0o10 => Self::RegularFile,
            0o12 => Self::Symlink,
            0o14 => Self::Socket,
            _ => Self::Unknown,
        }
    }
}

impl From<u8> for NodeType {
    fn from(value: u8) -> Self {
        Self::from_mode_type(value)
    }
}

bitflags::bitflags! {
    /// Inode permission mode.
    #[derive(Debug, Clone, Copy)]
    pub struct NodePermission: u16 {
        /// Set user ID on execution.
        const SET_UID = 0o4000;
        /// Set group ID on execution.
        const SET_GID = 0o2000;
        /// Sticky bit.
        const STICKY = 0o1000;

        /// Owner has read permission.
        const OWNER_READ = 0o400;
        /// Owner has write permission.
        const OWNER_WRITE = 0o200;
        /// Owner has execute permission.
        const OWNER_EXEC = 0o100;

        /// Group has read permission.
        const GROUP_READ = 0o40;
        /// Group has write permission.
        const GROUP_WRITE = 0o20;
        /// Group has execute permission.
        const GROUP_EXEC = 0o10;

        /// Others have read permission.
        const OTHER_READ = 0o4;
        /// Others have write permission.
        const OTHER_WRITE = 0o2;
        /// Others have execute permission.
        const OTHER_EXEC = 0o1;
    }
}

impl Default for NodePermission {
    fn default() -> Self {
        Self::from_bits_truncate(0o666)
    }
}

const NODE_TYPE_SHIFT: u16 = 12;
const NODE_TYPE_MASK: u16 = 0o170000;
const PERMISSION_MASK: u16 = 0o7777;

impl NodePermission {
    /// Returns the mode bits kept by legacy open-style creation.
    pub fn valid_mode_bits(self) -> Self {
        Self::from_bits_truncate(self.bits() & PERMISSION_MASK)
    }
}

/// Inode mode value encoded with Linux `umode_t` layout.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Umode(u16);

impl Umode {
    /// Creates a mode value from raw Linux mode bits.
    pub const fn from_bits(bits: u16) -> Self {
        Self(bits)
    }

    /// Creates a mode value from a node type and permission bits.
    pub const fn new(node_type: NodeType, permission: NodePermission) -> Self {
        Self(((node_type as u16) << NODE_TYPE_SHIFT) | (permission.bits() & PERMISSION_MASK))
    }

    /// Returns the raw Linux mode bits.
    pub const fn bits(self) -> u16 {
        self.0
    }

    /// Returns the file type encoded in this mode.
    pub const fn node_type(self) -> NodeType {
        NodeType::from_mode_type(((self.0 & NODE_TYPE_MASK) >> NODE_TYPE_SHIFT) as u8)
    }

    /// Returns the permission bits encoded in this mode.
    pub const fn permission(self) -> NodePermission {
        NodePermission::from_bits_truncate(self.0 & PERMISSION_MASK)
    }

    /// Returns this mode with new permission bits and the same file type.
    pub const fn with_permission(self, permission: NodePermission) -> Self {
        Self((self.0 & NODE_TYPE_MASK) | (permission.bits() & PERMISSION_MASK))
    }

    /// Returns this mode with a new file type and the same permission bits.
    pub const fn with_node_type(self, node_type: NodeType) -> Self {
        Self(((node_type as u16) << NODE_TYPE_SHIFT) | (self.0 & PERMISSION_MASK))
    }
}

/// Filesystem node metadata.
#[derive(Clone, Debug)]
pub struct Metadata {
    /// ID of device containing file
    pub device: u64,
    /// Inode number
    pub inode: u64,
    /// Number of hard links
    pub nlink: u64,
    /// File type and permission mode.
    pub mode: Umode,
    /// User ID of owner
    pub uid: u32,
    /// Group ID of owner
    pub gid: u32,
    /// Total size in bytes
    pub size: u64,
    /// Block size for filesystem I/O
    pub block_size: u64,
    /// Number of 512B blocks allocated
    pub blocks: u64,
    /// Device ID (if special file)
    pub rdev: DeviceId,

    /// Time of last access
    pub atime: Duration,
    /// Time of last modification
    pub mtime: Duration,
    /// Time of last status change
    pub ctime: Duration,
}

/// Filesystem node metadata update.
#[derive(Default, Clone, Debug)]
pub struct MetadataUpdate {
    /// File size
    pub size: Option<u64>,
    /// Permission mode
    pub mode: Option<NodePermission>,
    /// The owner (uid, gid)
    pub owner: Option<(u32, u32)>,

    /// Time of last access
    pub atime: Option<Duration>,
    /// Time of last modification
    pub mtime: Option<Duration>,
}

/// Requested timestamp value together with its authorization semantics.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SetattrTime {
    /// Set the timestamp to the current wall-clock value.
    Current(Duration),
    /// Set the timestamp to a caller-supplied value.
    Explicit(Duration),
}

impl SetattrTime {
    /// Returns the resolved timestamp written to the filesystem.
    pub const fn value(self) -> Duration {
        match self {
            Self::Current(value) | Self::Explicit(value) => value,
        }
    }
}

/// Device identifier (major/minor encoding).
#[derive(Default, Clone, PartialEq, Eq, Copy)]
pub struct DeviceId(pub u64);

impl DeviceId {
    /// Create a new device ID from major/minor numbers.
    pub const fn new(major: u32, minor: u32) -> Self {
        let major = major as u64;
        let minor = minor as u64;
        Self(
            (major & 0xffff_f000) << 32
                | (major & 0x0000_0fff) << 8
                | (minor & 0xffff_ff00) << 12
                | (minor & 0x0000_00ff),
        )
    }

    /// Return the major number.
    pub const fn major(&self) -> u32 {
        ((self.0 >> 32) & 0xffff_f000 | (self.0 >> 8) & 0x0000_0fff) as u32
    }

    /// Return the minor number.
    pub const fn minor(&self) -> u32 {
        ((self.0 >> 12) & 0xffff_ff00 | self.0 & 0x0000_00ff) as u32
    }
}

impl Debug for DeviceId {
    fn fmt(&self, f: &mut core::fmt::Formatter) -> core::fmt::Result {
        f.debug_struct("DeviceId")
            .field("major", &self.major())
            .field("minor", &self.minor())
            .finish()
    }
}

#[cfg(unittest)]
mod tests {
    use unittest::{assert_eq, def_test};

    use super::*;
    use crate::{SuperBlock, VfsError};

    #[def_test]
    fn test_node_type_conversion() {
        assert_eq!(NodeType::from(0o1), NodeType::Fifo);
        assert_eq!(NodeType::from(0o2), NodeType::CharacterDevice);
        assert_eq!(NodeType::from(0o4), NodeType::Directory);
        assert_eq!(NodeType::from(0o6), NodeType::BlockDevice);
        assert_eq!(NodeType::from(0o10), NodeType::RegularFile);
        assert_eq!(NodeType::from(0o12), NodeType::Symlink);
        assert_eq!(NodeType::from(0o14), NodeType::Socket);
        assert_eq!(NodeType::from(0o0), NodeType::Unknown);
        assert_eq!(NodeType::from(0o77), NodeType::Unknown);
    }

    #[def_test]
    fn test_node_permission_bitflags() {
        let rwx =
            NodePermission::OWNER_READ | NodePermission::OWNER_WRITE | NodePermission::OWNER_EXEC;
        assert!(rwx.contains(NodePermission::OWNER_READ));
        assert!(rwx.contains(NodePermission::OWNER_WRITE));
        assert!(rwx.contains(NodePermission::OWNER_EXEC));
        assert!(!rwx.contains(NodePermission::GROUP_READ));

        let default = NodePermission::default();
        assert!(default.contains(NodePermission::OWNER_READ));
        assert!(default.contains(NodePermission::OWNER_WRITE));
        assert!(!default.contains(NodePermission::OWNER_EXEC));
        assert!(default.contains(NodePermission::GROUP_READ));
        assert!(default.contains(NodePermission::GROUP_WRITE));
        assert!(default.contains(NodePermission::OTHER_READ));
        assert!(default.contains(NodePermission::OTHER_WRITE));

        let special = NodePermission::SET_UID | NodePermission::SET_GID | NodePermission::STICKY;
        assert!(special.contains(NodePermission::SET_UID));
        assert!(special.contains(NodePermission::SET_GID));
        assert!(special.contains(NodePermission::STICKY));
    }

    #[def_test]
    fn test_device_id_major_minor() {
        let dev1 = DeviceId::new(1, 2);
        assert_eq!(dev1.major(), 1);
        assert_eq!(dev1.minor(), 2);

        let dev2 = DeviceId::new(0x1234, 0x5678);
        assert_eq!(dev2.major(), 0x1234);
        assert_eq!(dev2.minor(), 0x5678);

        let dev3 = DeviceId::new(0, 0);
        assert_eq!(dev3.major(), 0);
        assert_eq!(dev3.minor(), 0);

        let dev4 = DeviceId::new(0xFFFFFFFF, 0xFFFFFFFF);
        assert_eq!(dev4.major(), 0xFFFFFFFF);
        assert_eq!(dev4.minor(), 0xFFFFFFFF);
    }

    #[def_test]
    fn test_vfs_error_types() {
        let _not_found = VfsError::NotFound;
        let _already_exists = VfsError::AlreadyExists;
        let _not_a_directory = VfsError::NotADirectory;
        let _is_a_directory = VfsError::IsADirectory;
        let _directory_not_empty = VfsError::DirectoryNotEmpty;
        let _permission_denied = VfsError::PermissionDenied;
        let _invalid_input = VfsError::InvalidInput;
    }

    #[def_test]
    fn test_super_block_handle_type_constraints() {
        fn assert_traits<T: Send + Sync + Clone>() {}
        assert_traits::<alloc::sync::Arc<SuperBlock>>();
    }
}
