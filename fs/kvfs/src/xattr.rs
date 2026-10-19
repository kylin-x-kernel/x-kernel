// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Extended-attribute names, flags, and generic VFS policy.

use alloc::vec::Vec;

use kcred::Cred;
use kerrno::LinuxError;

use crate::{NodePermission, NodeType, Path, Permission, VfsError, VfsInode, VfsResult};

/// Maximum byte length of a Linux extended-attribute name, excluding NUL.
pub const XATTR_NAME_MAX: usize = 255;

const USER_PREFIX: &[u8] = b"user.";
const TRUSTED_PREFIX: &[u8] = b"trusted.";
const SECURITY_PREFIX: &[u8] = b"security.";
const SYSTEM_PREFIX: &[u8] = b"system.";

bitflags::bitflags! {
    /// Creation policy for an extended-attribute update.
    #[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
    pub struct XattrSetFlags: u32 {
        /// Fail with `EEXIST` when the attribute already exists.
        const CREATE = 1;
        /// Fail with `ENODATA` when the attribute does not exist.
        const REPLACE = 2;
    }
}

/// A validated, kernel-owned extended-attribute name.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct XattrName(Vec<u8>);

impl XattrName {
    /// Validates and takes ownership of a complete xattr name.
    ///
    /// The name includes its namespace prefix and must be nonempty, contain no
    /// embedded NUL, and fit the Linux `XATTR_NAME_MAX` limit.
    pub fn new(name: Vec<u8>) -> VfsResult<Self> {
        if name.is_empty() || name.len() > XATTR_NAME_MAX || name.contains(&0) {
            return Err(VfsError::from(LinuxError::ERANGE));
        }
        Ok(Self(name))
    }

    /// Returns the complete xattr name, including its namespace prefix.
    pub fn as_bytes(&self) -> &[u8] {
        &self.0
    }
}

/// A validated borrowed xattr name, optionally split into two byte slices.
///
/// The split form lets filesystem bridges add a namespace prefix without
/// allocating a temporary complete name.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct XattrNameRef<'a> {
    first: &'a [u8],
    second: &'a [u8],
}

impl<'a> XattrNameRef<'a> {
    /// Validates a borrowed complete xattr name.
    pub fn new(name: &'a [u8]) -> VfsResult<Self> {
        Self::from_parts(name, &[])
    }

    /// Validates a borrowed xattr name assembled from two adjacent parts.
    pub fn from_parts(first: &'a [u8], second: &'a [u8]) -> VfsResult<Self> {
        let len = first
            .len()
            .checked_add(second.len())
            .ok_or_else(|| VfsError::from(LinuxError::ERANGE))?;
        if len == 0 || len > XATTR_NAME_MAX || first.contains(&0) || second.contains(&0) {
            return Err(VfsError::from(LinuxError::ERANGE));
        }
        Ok(Self { first, second })
    }

    /// Returns the complete encoded length, excluding the trailing NUL.
    pub const fn encoded_len(self) -> usize {
        self.first.len() + self.second.len()
    }

    /// Appends the complete name to an output buffer.
    pub fn append_to(self, output: &mut Vec<u8>) {
        output.extend_from_slice(self.first);
        output.extend_from_slice(self.second);
    }

    fn starts_with(self, prefix: &[u8]) -> bool {
        if self.first.len() >= prefix.len() {
            self.first.starts_with(prefix)
        } else {
            prefix.starts_with(self.first) && self.second.starts_with(&prefix[self.first.len()..])
        }
    }

    fn is_trusted(self) -> bool {
        self.starts_with(TRUSTED_PREFIX)
    }
}

/// Receives borrowed complete xattr names from an inode implementation.
pub trait XattrNameSink {
    /// Consumes one name before its borrowed storage is released.
    ///
    /// # Errors
    ///
    /// Returns an error when the consumer cannot accept the name.
    fn emit(&mut self, name: XattrNameRef<'_>) -> VfsResult<()>;
}

struct CredentialFilteringSink<'a> {
    inner: &'a mut dyn XattrNameSink,
    is_privileged: bool,
}

impl XattrNameSink for CredentialFilteringSink<'_> {
    fn emit(&mut self, name: XattrNameRef<'_>) -> VfsResult<()> {
        if !self.is_privileged && name.is_trusted() {
            return Ok(());
        }
        self.inner.emit(name)
    }
}

#[derive(Clone, Copy)]
enum XattrAccess {
    Read,
    Write,
}

impl XattrAccess {
    const fn permission(self) -> Permission {
        match self {
            Self::Read => Permission::MAY_READ,
            Self::Write => Permission::MAY_WRITE,
        }
    }

    fn namespace_error(self) -> VfsError {
        match self {
            Self::Read => VfsError::from(LinuxError::ENODATA),
            Self::Write => VfsError::OperationNotPermitted,
        }
    }
}

fn check_xattr_permission(
    inode: &VfsInode,
    name: &XattrName,
    access: XattrAccess,
    cred: &Cred,
) -> VfsResult<()> {
    if matches!(access, XattrAccess::Write)
        && inode
            .flags()
            .intersects(crate::NodeFlags::IMMUTABLE | crate::NodeFlags::APPEND_ONLY)
    {
        return Err(VfsError::OperationNotPermitted);
    }

    let name = name.as_bytes();

    // Linux delegates security.* authorization to LSM hooks. Until KVFS has
    // that hook boundary, require the privileged-credential approximation for
    // mutations instead of letting an unprivileged caller forge security data.
    if name.starts_with(SECURITY_PREFIX) {
        return if matches!(access, XattrAccess::Write) && !cred.is_privileged() {
            Err(VfsError::OperationNotPermitted)
        } else {
            Ok(())
        };
    }

    // Linux leaves system.* authorization to the filesystem and ACL layers
    // instead of applying generic inode DAC here.
    if name.starts_with(SYSTEM_PREFIX) {
        return Ok(());
    }

    if name.starts_with(TRUSTED_PREFIX) {
        return if cred.is_privileged() {
            Ok(())
        } else {
            Err(access.namespace_error())
        };
    }

    if name.starts_with(USER_PREFIX) {
        match inode.node_type() {
            NodeType::RegularFile | NodeType::Socket => {}
            NodeType::Directory => {
                let metadata = inode.metadata();
                if matches!(access, XattrAccess::Write)
                    && metadata.mode.permission().contains(NodePermission::STICKY)
                    && cred.fsuid() != metadata.uid
                    && !cred.is_privileged()
                {
                    return Err(VfsError::OperationNotPermitted);
                }
            }
            _ => return Err(access.namespace_error()),
        }
    }

    inode.permission(access.permission(), cred)
}

impl Path {
    /// Reads one extended attribute after applying namespace and inode policy.
    pub fn get_xattr(&self, name: &XattrName, cred: &Cred) -> VfsResult<Vec<u8>> {
        let inode = self.inode();
        check_xattr_permission(&inode, name, XattrAccess::Read, cred)?;
        inode.get_xattr(self.dentry(), name)
    }

    /// Streams complete names of extended attributes visible to `cred`.
    ///
    /// `trusted.*` names are filtered before reaching `sink` for
    /// unprivileged credentials.
    pub fn list_xattrs(&self, cred: &Cred, sink: &mut dyn XattrNameSink) -> VfsResult<()> {
        let inode = self.inode();
        let mut filtering_sink = CredentialFilteringSink {
            inner: sink,
            is_privileged: cred.is_privileged(),
        };
        inode.list_xattrs(self.dentry(), &mut filtering_sink)
    }

    /// Creates or replaces one extended attribute.
    ///
    /// Mutating `security.*` requires a privileged credential until KVFS has
    /// an LSM authorization hook.
    pub fn set_xattr(
        &self,
        name: &XattrName,
        value: &[u8],
        flags: XattrSetFlags,
        cred: &Cred,
    ) -> VfsResult<()> {
        self.check_writable_mount()?;
        let inode = self.inode();
        check_xattr_permission(&inode, name, XattrAccess::Write, cred)?;
        let _data_guard = inode.lock_data();
        inode.set_xattr(self.dentry(), name, value, flags)
    }

    /// Removes one extended attribute.
    ///
    /// Removing `security.*` requires a privileged credential until KVFS has
    /// an LSM authorization hook.
    pub fn remove_xattr(&self, name: &XattrName, cred: &Cred) -> VfsResult<()> {
        self.check_writable_mount()?;
        let inode = self.inode();
        check_xattr_permission(&inode, name, XattrAccess::Write, cred)?;
        let _data_guard = inode.lock_data();
        inode.remove_xattr(self.dentry(), name)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/xattr.rs");
