// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! VFS permission request and DAC checks.

use kcred::AccessCredentials;
use kerrno::{KError, KResult};

use crate::{Metadata, NodePermission, NodeType};

bitflags::bitflags! {
    /// Requested access permissions for a filesystem node.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub struct Permission: u16 {
        /// Execute a file or search a directory.
        const MAY_EXEC = 0x0001;
        /// Write to a file or modify a directory.
        const MAY_WRITE = 0x0002;
        /// Read from a file or directory.
        const MAY_READ = 0x0004;
        /// Append to a file.
        const MAY_APPEND = 0x0008;
        /// Check access permission.
        const MAY_ACCESS = 0x0010;
        /// Open a file.
        const MAY_OPEN = 0x0020;
        /// Change current directory to this directory.
        const MAY_CHDIR = 0x0040;
    }
}

/// Checks DAC permissions for a filesystem node.
///
/// Follows the POSIX owner, group, and other permission cascade. The caller
/// supplies the credential IDs used for the check, such as fsuid/fsgid for VFS
/// operations or real IDs for `access(2)`-style checks.
pub fn check_permission(
    metadata: &Metadata,
    permission: Permission,
    credentials: &AccessCredentials,
) -> KResult<()> {
    let permission = permission
        .intersection(Permission::MAY_READ | Permission::MAY_WRITE | Permission::MAY_EXEC);
    let mode = metadata.mode.permission();
    if permission.is_empty() || has_all_requested_bits(mode, permission) {
        return Ok(());
    }

    if credentials.uid() == 0 {
        if permission.contains(Permission::MAY_EXEC)
            && metadata.mode.node_type() != NodeType::Directory
            && !has_any_exec_bit(metadata)
        {
            return Err(KError::PermissionDenied);
        }
        return Ok(());
    }

    let is_allowed = if credentials.uid() == metadata.uid {
        owner_allows(mode, permission)
    } else if credentials.has_group(metadata.gid) {
        group_allows(mode, permission)
    } else {
        other_allows(mode, permission)
    };

    if !is_allowed {
        return Err(KError::PermissionDenied);
    }

    Ok(())
}

/// Returns the [`Permission`] implied by read and write open access.
pub fn open_access_to_permission(read: bool, write: bool) -> Permission {
    let mut permission = Permission::empty();
    if read {
        permission |= Permission::MAY_READ;
    }
    if write {
        permission |= Permission::MAY_WRITE;
    }
    permission
}

fn has_any_exec_bit(metadata: &Metadata) -> bool {
    metadata.mode.permission().intersects(
        NodePermission::OWNER_EXEC | NodePermission::GROUP_EXEC | NodePermission::OTHER_EXEC,
    )
}

fn has_all_requested_bits(mode: NodePermission, permission: Permission) -> bool {
    owner_allows(mode, permission)
        && group_allows(mode, permission)
        && other_allows(mode, permission)
}

fn owner_allows(mode: NodePermission, permission: Permission) -> bool {
    allows(
        mode,
        permission,
        NodePermission::OWNER_READ,
        NodePermission::OWNER_WRITE,
        NodePermission::OWNER_EXEC,
    )
}

fn group_allows(mode: NodePermission, permission: Permission) -> bool {
    allows(
        mode,
        permission,
        NodePermission::GROUP_READ,
        NodePermission::GROUP_WRITE,
        NodePermission::GROUP_EXEC,
    )
}

fn other_allows(mode: NodePermission, permission: Permission) -> bool {
    allows(
        mode,
        permission,
        NodePermission::OTHER_READ,
        NodePermission::OTHER_WRITE,
        NodePermission::OTHER_EXEC,
    )
}

fn allows(
    mode: NodePermission,
    permission: Permission,
    read_bit: NodePermission,
    write_bit: NodePermission,
    exec_bit: NodePermission,
) -> bool {
    (!permission.contains(Permission::MAY_READ) || mode.contains(read_bit))
        && (!permission.contains(Permission::MAY_WRITE) || mode.contains(write_bit))
        && (!permission.contains(Permission::MAY_EXEC) || mode.contains(exec_bit))
}

#[cfg(unittest)]
mod tests {
    use alloc::sync::Arc;
    use core::time::Duration;

    use kcred::AccessCredentials;
    use unittest::def_test;

    use super::{Permission, check_permission};
    use crate::{DeviceId, Metadata, NodePermission, NodeType, Umode};

    fn creds(uid: u32, gid: u32, groups: &[u32]) -> AccessCredentials {
        AccessCredentials::new(uid, gid, Arc::from(groups))
    }

    fn meta(uid: u32, gid: u32, mode: u16) -> Metadata {
        Metadata {
            device: 0,
            inode: 1,
            nlink: 1,
            mode: Umode::new(
                NodeType::RegularFile,
                NodePermission::from_bits_truncate(mode),
            ),
            uid,
            gid,
            size: 0,
            block_size: 512,
            blocks: 1,
            rdev: DeviceId::default(),
            atime: Duration::ZERO,
            mtime: Duration::ZERO,
            ctime: Duration::ZERO,
        }
    }

    #[def_test]
    fn test_owner_read_allowed() {
        let m = meta(1000, 1000, 0o400);
        assert!(check_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_ok());
    }

    #[def_test]
    fn test_owner_read_denied_when_bit_clear() {
        let m = meta(1000, 1000, 0o000);
        assert!(check_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_err());
    }

    #[def_test]
    fn test_owner_write_allowed() {
        let m = meta(1000, 1000, 0o200);
        assert!(check_permission(&m, Permission::MAY_WRITE, &creds(1000, 2000, &[])).is_ok());
    }

    #[def_test]
    fn test_owner_exec_allowed() {
        let m = meta(1000, 1000, 0o100);
        assert!(check_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_ok());
    }

    #[def_test]
    fn test_group_read_allowed_via_gid() {
        let m = meta(0, 2000, 0o040);
        assert!(check_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_ok());
    }

    #[def_test]
    fn test_group_read_allowed_via_supplementary() {
        let m = meta(0, 3000, 0o040);
        assert!(check_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[3000])).is_ok());
    }

    #[def_test]
    fn test_group_read_denied_when_not_member() {
        let m = meta(0, 3000, 0o040);
        assert!(check_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_err());
    }

    #[def_test]
    fn test_other_read_allowed() {
        let m = meta(0, 0, 0o004);
        assert!(check_permission(&m, Permission::MAY_READ, &creds(1000, 2000, &[])).is_ok());
    }

    #[def_test]
    fn test_other_write_denied_when_bit_clear() {
        let m = meta(0, 0, 0o004);
        assert!(check_permission(&m, Permission::MAY_WRITE, &creds(1000, 2000, &[])).is_err());
    }

    #[def_test]
    fn test_root_read_bypasses_zero_mode() {
        let m = meta(1000, 1000, 0o000);
        assert!(check_permission(&m, Permission::MAY_READ, &creds(0, 0, &[])).is_ok());
    }

    #[def_test]
    fn test_root_write_bypasses_zero_mode() {
        let m = meta(1000, 1000, 0o000);
        assert!(check_permission(&m, Permission::MAY_WRITE, &creds(0, 0, &[])).is_ok());
    }

    #[def_test]
    fn test_root_exec_denied_when_no_exec_bit() {
        let m = meta(1000, 1000, 0o666);
        assert!(check_permission(&m, Permission::MAY_EXEC, &creds(0, 0, &[])).is_err());
    }

    #[def_test]
    fn test_root_dir_search_bypasses_missing_exec_bit() {
        let mut m = meta(1000, 1000, 0o000);
        m.mode = m.mode.with_node_type(NodeType::Directory);
        assert!(check_permission(&m, Permission::MAY_EXEC, &creds(0, 0, &[])).is_ok());
    }

    #[def_test]
    fn test_root_exec_allowed_when_any_exec_bit_set() {
        let m = meta(1000, 1000, 0o711);
        assert!(check_permission(&m, Permission::MAY_EXEC, &creds(0, 0, &[])).is_ok());
    }

    #[def_test]
    fn test_owner_rw_allowed() {
        let m = meta(1000, 1000, 0o600);
        assert!(
            check_permission(
                &m,
                Permission::MAY_READ | Permission::MAY_WRITE,
                &creds(1000, 2000, &[])
            )
            .is_ok()
        );
    }

    #[def_test]
    fn test_empty_request_always_allowed() {
        let m = meta(1000, 1000, 0o000);
        assert!(check_permission(&m, Permission::empty(), &creds(1000, 2000, &[])).is_ok());
    }

    #[def_test]
    fn test_group_write_allowed_via_gid() {
        let m = meta(0, 2000, 0o020);
        assert!(check_permission(&m, Permission::MAY_WRITE, &creds(1000, 2000, &[])).is_ok());
    }

    #[def_test]
    fn test_group_exec_allowed_via_supplementary() {
        let m = meta(0, 3000, 0o010);
        assert!(check_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[3000])).is_ok());
    }

    #[def_test]
    fn test_other_exec_allowed() {
        let m = meta(0, 0, 0o001);
        assert!(check_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_ok());
    }

    #[def_test]
    fn test_other_exec_denied_when_bit_clear() {
        let m = meta(0, 0, 0o006);
        assert!(check_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_err());
    }

    #[def_test]
    fn test_dir_search_allowed_for_owner() {
        let mut m = meta(1000, 1000, 0o100);
        m.mode = m.mode.with_node_type(NodeType::Directory);
        assert!(check_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_ok());
    }

    #[def_test]
    fn test_dir_search_denied_when_exec_bit_clear() {
        let mut m = meta(1000, 1000, 0o600);
        m.mode = m.mode.with_node_type(NodeType::Directory);
        assert!(check_permission(&m, Permission::MAY_EXEC, &creds(1000, 2000, &[])).is_err());
    }

    #[def_test]
    fn test_owner_bits_used_when_uid_matches_even_if_group_denies() {
        let m = meta(1000, 1000, 0o040);
        assert!(check_permission(&m, Permission::MAY_READ, &creds(1000, 1000, &[])).is_err());
    }
}
