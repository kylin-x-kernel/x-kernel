// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, vec};

use kcred::Cred;
use kerrno::LinuxError;
use unittest::{assert, assert_eq, def_test};

use super::{XattrAccess, XattrName, XattrNameRef, check_xattr_permission};
use crate::{
    FileOperations, InodeOperations, NodeFlags, NodePermission, NodeType, Umode, VfsInode,
    VfsInodeInit,
};

struct TestOps;

impl InodeOperations for TestOps {}
impl FileOperations for TestOps {}

fn inode_with_flags(
    node_type: NodeType,
    permission: u16,
    uid: u32,
    flags: NodeFlags,
) -> Arc<VfsInode> {
    let init = VfsInodeInit::new(
        1,
        0,
        Umode::new(node_type, NodePermission::from_bits_truncate(permission)),
    )
    .with_owner_links_and_rdev(uid, uid, 1, Default::default());
    if node_type == NodeType::Directory {
        VfsInode::new_dir_with_defaults(flags, init)
    } else {
        VfsInode::new_file_with_flags(Arc::new(TestOps), flags, init)
    }
}

fn inode(node_type: NodeType, permission: u16, uid: u32) -> Arc<VfsInode> {
    inode_with_flags(node_type, permission, uid, NodeFlags::empty())
}

#[def_test]
fn validates_xattr_names() {
    assert_eq!(
        XattrName::new(vec![b'a'; super::XATTR_NAME_MAX])
            .unwrap()
            .as_bytes()
            .len(),
        super::XATTR_NAME_MAX
    );
    assert!(matches!(
        XattrName::new(vec![b'a'; super::XATTR_NAME_MAX + 1]),
        Err(err) if LinuxError::from(err) == LinuxError::ERANGE
    ));
    assert!(matches!(
        XattrName::new(vec![]),
        Err(err) if LinuxError::from(err) == LinuxError::ERANGE
    ));

    let split = XattrNameRef::from_parts(b"trusted.", b"test").unwrap();
    assert_eq!(split.encoded_len(), b"trusted.test".len());
    assert!(split.is_trusted());
    assert!(matches!(
        XattrNameRef::from_parts(b"user.", b"bad\0name"),
        Err(err) if LinuxError::from(err) == LinuxError::ERANGE
    ));
}

#[def_test]
fn user_namespace_follows_inode_type_and_dac() {
    let name = XattrName::new(b"user.test".to_vec()).unwrap();
    let owner = Cred::new(1000, 1000);
    let other = Cred::new(2000, 2000);
    let regular = inode(NodeType::RegularFile, 0o600, 1000);
    assert_eq!(
        check_xattr_permission(&regular, &name, XattrAccess::Write, &owner),
        Ok(())
    );
    assert!(matches!(
        check_xattr_permission(&regular, &name, XattrAccess::Read, &other),
        Err(err) if LinuxError::from(err) == LinuxError::EACCES
    ));

    let group_or_other_writable = inode(NodeType::RegularFile, 0o666, 1000);
    assert_eq!(
        check_xattr_permission(&group_or_other_writable, &name, XattrAccess::Write, &other),
        Ok(())
    );

    let socket = inode(NodeType::Socket, 0o600, 1000);
    assert_eq!(
        check_xattr_permission(&socket, &name, XattrAccess::Write, &owner),
        Ok(())
    );

    let symlink = inode(NodeType::Symlink, 0o777, 1000);
    assert!(matches!(
        check_xattr_permission(&symlink, &name, XattrAccess::Write, &owner),
        Err(err) if LinuxError::from(err) == LinuxError::EPERM
    ));
    assert!(matches!(
        check_xattr_permission(&symlink, &name, XattrAccess::Read, &owner),
        Err(err) if LinuxError::from(err) == LinuxError::ENODATA
    ));
}

#[def_test]
fn trusted_and_sticky_user_namespaces_require_privilege_or_owner() {
    let root = Cred::new(0, 0);
    let other = Cred::new(2000, 2000);
    let trusted = XattrName::new(b"trusted.test".to_vec()).unwrap();
    let regular = inode(NodeType::RegularFile, 0o666, 1000);
    assert_eq!(
        check_xattr_permission(&regular, &trusted, XattrAccess::Write, &root),
        Ok(())
    );
    assert!(matches!(
        check_xattr_permission(&regular, &trusted, XattrAccess::Read, &other),
        Err(err) if LinuxError::from(err) == LinuxError::ENODATA
    ));

    let user = XattrName::new(b"user.test".to_vec()).unwrap();
    let sticky = inode(NodeType::Directory, 0o1777, 1000);
    assert!(matches!(
        check_xattr_permission(&sticky, &user, XattrAccess::Write, &other),
        Err(err) if LinuxError::from(err) == LinuxError::EPERM
    ));
}

#[def_test]
fn security_namespace_mutations_require_privilege() {
    let root = Cred::new(0, 0);
    let other = Cred::new(2000, 2000);
    let security = XattrName::new(b"security.test".to_vec()).unwrap();
    let regular = inode(NodeType::RegularFile, 0o000, 1000);

    assert_eq!(
        check_xattr_permission(&regular, &security, XattrAccess::Write, &root),
        Ok(())
    );
    assert!(matches!(
        check_xattr_permission(&regular, &security, XattrAccess::Write, &other),
        Err(err) if LinuxError::from(err) == LinuxError::EPERM
    ));
    assert_eq!(
        check_xattr_permission(&regular, &security, XattrAccess::Read, &other),
        Ok(())
    );
}

#[def_test]
fn immutable_and_append_only_inodes_reject_all_xattr_mutations() {
    let root = Cred::new(0, 0);
    let security = XattrName::new(b"security.test".to_vec()).unwrap();
    let system = XattrName::new(b"system.test".to_vec()).unwrap();

    for flags in [NodeFlags::IMMUTABLE, NodeFlags::APPEND_ONLY] {
        let regular = inode_with_flags(NodeType::RegularFile, 0o600, 0, flags);
        for name in [&security, &system] {
            assert!(matches!(
                check_xattr_permission(&regular, name, XattrAccess::Write, &root),
                Err(err) if LinuxError::from(err) == LinuxError::EPERM
            ));
        }
        assert_eq!(
            check_xattr_permission(&regular, &security, XattrAccess::Read, &root),
            Ok(())
        );
    }
}
