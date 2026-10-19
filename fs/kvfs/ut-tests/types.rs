// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

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
    let rwx = NodePermission::OWNER_READ | NodePermission::OWNER_WRITE | NodePermission::OWNER_EXEC;
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
fn mknod_node_type_decodes_linux_type_field() {
    assert_eq!(
        Umode::from_bits(0o600).mknod_node_type(),
        NodeType::RegularFile
    );
    assert_eq!(
        Umode::from_bits(0o100600).mknod_node_type(),
        NodeType::RegularFile
    );
    assert_eq!(Umode::from_bits(0o010600).mknod_node_type(), NodeType::Fifo);
    assert_eq!(
        Umode::from_bits(0o020600).mknod_node_type(),
        NodeType::CharacterDevice
    );
    assert_eq!(
        Umode::from_bits(0o060600).mknod_node_type(),
        NodeType::BlockDevice
    );
    assert_eq!(
        Umode::from_bits(0o140600).mknod_node_type(),
        NodeType::Socket
    );
    assert_eq!(
        Umode::from_bits(0o040700).mknod_node_type(),
        NodeType::Directory
    );
    assert_eq!(
        Umode::from_bits(0o120777).mknod_node_type(),
        NodeType::Symlink
    );
    assert_eq!(
        Umode::from_bits(0o030600).mknod_node_type(),
        NodeType::Unknown
    );
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
