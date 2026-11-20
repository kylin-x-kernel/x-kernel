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
fn capability_cache_ignores_a_stale_observation() {
    // The revocation protocol publishes a cached absence only while no other
    // capability operation intervened: a compare-and-swap from an observation
    // that a later `setxattr` already superseded must fail.
    let inode = inode(NodeType::RegularFile, 0o644, 0);
    let stale = inode.file_capability_snapshot();
    inode.mark_file_capability(crate::FileCapabilityState::Present);
    assert_eq!(
        inode.file_capability_state(),
        crate::FileCapabilityState::Present
    );
    assert!(!inode.try_mark_file_capability(stale, crate::FileCapabilityState::Absent));
    assert_eq!(
        inode.file_capability_state(),
        crate::FileCapabilityState::Present
    );

    // A stale observation whose *state* still matches must also fail: the
    // intervening set-then-remove cycle returned the cache to `Absent` but
    // changed the generation. A comparison that ignored the generation would
    // accept this observation and mask the race the protocol exists to stop.
    inode.mark_file_capability(crate::FileCapabilityState::Absent);
    let stale_absent = inode.file_capability_snapshot();
    inode.mark_file_capability(crate::FileCapabilityState::Present);
    inode.mark_file_capability(crate::FileCapabilityState::Absent);
    assert!(!inode.try_mark_file_capability(stale_absent, crate::FileCapabilityState::Absent));

    // A fresh observation may publish the absence.
    let current = inode.file_capability_snapshot();
    assert!(inode.try_mark_file_capability(current, crate::FileCapabilityState::Absent));
    assert_eq!(
        inode.file_capability_state(),
        crate::FileCapabilityState::Absent
    );
}

#[def_test]
fn removal_publication_keeps_a_concurrent_present() {
    // The successful-remove path snapshots the generation before the backend
    // delete. If a concurrent setxattr marks Present in between, publishing
    // Absent must lose and degrade to Unknown, never hide the new attribute.
    let inode = inode(NodeType::RegularFile, 0o644, 0);
    let observed = inode.file_capability_snapshot();
    inode.mark_file_capability(crate::FileCapabilityState::Present);
    super::publish_capability_removal(&inode, observed);
    assert_eq!(
        inode.file_capability_state(),
        crate::FileCapabilityState::Unknown
    );

    // An uncontested removal may publish Absent.
    let fresh = inode.file_capability_snapshot();
    super::publish_capability_removal(&inode, fresh);
    assert_eq!(
        inode.file_capability_state(),
        crate::FileCapabilityState::Absent
    );
}

#[def_test]
fn inode_tracks_extended_attribute_storage() {
    // The backend reports attribute storage at load and the VFS keeps the flag
    // set once an attribute is installed, which is the `S_NOSEC`-style fast
    // path an ordinary content write relies on.
    let plain = inode(NodeType::RegularFile, 0o644, 0);
    assert!(!plain.has_extended_attributes());
    assert_eq!(
        plain.file_capability_state(),
        crate::FileCapabilityState::Unknown
    );
    plain.mark_extended_attributes();
    assert!(plain.has_extended_attributes());

    let init = VfsInodeInit::new(
        7,
        0,
        Umode::new(
            NodeType::RegularFile,
            NodePermission::from_bits_truncate(0o644),
        ),
    )
    .with_extended_attributes(true);
    let reported = VfsInode::new_file_with_flags(Arc::new(TestOps), NodeFlags::empty(), init);
    assert!(reported.has_extended_attributes());
}

#[def_test]
fn file_capability_attribute_is_gated_by_setfcap() {
    // Linux gates `security.capability` on CAP_SETFCAP rather than
    // CAP_SYS_ADMIN, so a packaging tool can manage file capabilities without
    // the broader authority over every security.* attribute.
    let packaging = {
        let mut cred = Cred::root();
        let bit = kcred::CapSet::from_bits(1 << kcred::CAP_SETFCAP);
        cred.set_capabilities(kcred::CapSets::new(bit, bit, kcred::CapSet::EMPTY))
            .unwrap();
        cred
    };
    assert!(!packaging.has_sys_admin());
    let capability = XattrName::new(b"security.capability".to_vec()).unwrap();
    let regular = inode(NodeType::RegularFile, 0o000, 1000);
    assert_eq!(
        check_xattr_permission(&regular, &capability, XattrAccess::Write, &packaging),
        Ok(())
    );

    // Without CAP_SETFCAP the write is refused, and an unrelated security.*
    // attribute still needs CAP_SYS_ADMIN.
    let other = Cred::new(2000, 2000);
    assert!(matches!(
        check_xattr_permission(&regular, &capability, XattrAccess::Write, &other),
        Err(err) if LinuxError::from(err) == LinuxError::EPERM
    ));
    let selinux = XattrName::new(b"security.selinux".to_vec()).unwrap();
    assert!(matches!(
        check_xattr_permission(&regular, &selinux, XattrAccess::Write, &packaging),
        Err(err) if LinuxError::from(err) == LinuxError::EPERM
    ));

    // Reads stay unrestricted, as they are for every security.* name.
    assert_eq!(
        check_xattr_permission(&regular, &capability, XattrAccess::Read, &other),
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
