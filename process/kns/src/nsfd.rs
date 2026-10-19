// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace file descriptor — a first-class fd that pins one namespace.
//!
//! A [`NsFd`] holds a strong reference to exactly one namespace object, so the
//! namespace stays alive (and its identity stays stable) while the fd is open,
//! matching Linux `nsfs` semantics. The fd is created by opening
//! `/proc/<pid>/ns/<type>` (see the procfs `ThreadNsDir` nodes) and consumed by
//! `setns(2)`. Data reads and writes are always rejected.
//!
//! The display path follows the Linux `anon_inode:[<type>_ns]` convention so
//! `/proc/self/fd/<n>` renders identically to a native nsfs fd.

use alloc::sync::Arc;

use anon_inodefs::AnonInodeFs;
use kcgroup::CgroupNamespace;
use kcred::{Cred, UserNamespace};
use kerrno::{KError, KResult};
use kvfs::{FMode, FileOperations, OpenFlags, VfsFile};

use crate::{
    IpcNamespace, MntNamespace, NamespaceFlags, NamespaceType, NetNamespace, PidNamespace,
    UtsNamespace,
};

/// Anonymous-inode display name per namespace type.
///
/// Follows the tgoskits/Linux naming: the namespace type name plus the `_ns`
/// suffix inside the `anon_inode:[...]` bracket, except PID namespaces which
/// expose the child-side role (`pid_ns_for_children`).
pub(crate) fn anon_inode_name(kind: NamespaceType) -> &'static str {
    match kind {
        NamespaceType::Mnt => "[mnt_ns]",
        NamespaceType::Uts => "[uts_ns]",
        NamespaceType::Ipc => "[ipc_ns]",
        NamespaceType::User => "[user_ns]",
        NamespaceType::Pid => "[pid_ns_for_children]",
        NamespaceType::Net => "[net_ns]",
        NamespaceType::Cgroup => "[cgroup_ns]",
        NamespaceType::Time => "[time_ns]",
    }
}

/// Filesystem namespace identity exported through `/proc/<pid>/ns/<type>`.
///
/// Holds the reference to exactly one namespace. `Mnt` keeps only the kernel
/// mount namespace; the VFS `FsStruct` retarget for `CLONE_NEWNS` is computed
/// at `setns` application time from the target namespace's visible root.
#[derive(Clone)]
pub enum NsFd {
    /// Hostname/domainname namespace.
    Uts(Arc<UtsNamespace>),
    /// System V IPC identity namespace.
    Ipc(Arc<IpcNamespace>),
    /// Mount-tree namespace.
    Mnt(Arc<MntNamespace>),
    /// PID namespace used for future children.
    Pid(Arc<PidNamespace>),
    /// Network namespace.
    Net(Arc<NetNamespace>),
    /// Credential owner namespace.
    User(Arc<UserNamespace>),
    /// Cgroup hierarchy view.
    Cgroup(Arc<CgroupNamespace>),
}

impl NsFd {
    /// Returns the namespace type carried by this fd.
    pub const fn ns_type(&self) -> NamespaceType {
        match self {
            Self::Uts(_) => NamespaceType::Uts,
            Self::Ipc(_) => NamespaceType::Ipc,
            Self::Mnt(_) => NamespaceType::Mnt,
            Self::Pid(_) => NamespaceType::Pid,
            Self::Net(_) => NamespaceType::Net,
            Self::User(_) => NamespaceType::User,
            Self::Cgroup(_) => NamespaceType::Cgroup,
        }
    }

    /// Returns the `CLONE_NEW*` creation flag that requests this namespace type.
    pub const fn namespace_flag(&self) -> NamespaceFlags {
        match self {
            Self::Uts(_) => NamespaceFlags::NEWUTS,
            Self::Ipc(_) => NamespaceFlags::NEWIPC,
            Self::Mnt(_) => NamespaceFlags::NEWNS,
            Self::Pid(_) => NamespaceFlags::NEWPID,
            Self::Net(_) => NamespaceFlags::NEWNET,
            Self::User(_) => NamespaceFlags::NEWUSER,
            Self::Cgroup(_) => NamespaceFlags::NEWCGROUP,
        }
    }

    /// Returns the fd's display path (`anon_inode:[<type>_ns]`).
    pub fn display_path(&self) -> alloc::borrow::Cow<'static, str> {
        alloc::borrow::Cow::Borrowed(anon_inode_name(self.ns_type()))
    }

    /// Wraps the namespace reference in an anonymous-inode file description.
    ///
    /// `cred` is captured as the new file's immutable open credential.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for unknown open-flag bits; forwards
    /// anonymous-inode file construction errors.
    pub fn new_file(&self, open_flags: u32, cred: Arc<Cred>) -> KResult<Arc<VfsFile>> {
        let open_flags = OpenFlags::from_bits(open_flags).ok_or(KError::InvalidInput)?;
        let name = anon_inode_name(self.ns_type());
        AnonInodeFs::global().get_file(
            name,
            Arc::new(NsFdFops),
            Arc::new((*self).clone()),
            FMode::READ,
            open_flags,
            cred,
        )
    }

    /// Returns the namespace fd object attached to a namespace file.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` if the file has no `NsFd` private data.
    pub fn from_file(file: &VfsFile) -> KResult<Arc<Self>> {
        file.private_data_get::<Self>()
            .ok_or(KError::BadFileDescriptor)
    }
}

/// File operations shared by all namespace fds.
///
/// Reads and writes are always denied; the fd exists only as a namespace
/// capability handle consumed by `setns(2)`, matching Linux (reads on an nsfs
/// file return EOF, writes fail with `EINVAL`).
struct NsFdFops;

impl FileOperations for NsFdFops {
    fn release(&self, _inode: &kvfs::VfsInode, _file: &VfsFile) -> KResult<()> {
        Ok(())
    }
}

unittest::path_mod_test!(tests_nsfd, "../ut-tests/nsfd.rs");
