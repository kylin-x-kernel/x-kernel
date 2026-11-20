// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Materialization of the namespaces selected by `CLONE_NEW*` flags.
//!
//! `clone(2)` and `unshare(2)` differ in what they do with the *current*
//! process, but both must create exactly the same objects for the requested
//! flags. [`NamespacePlan`] owns that one construction path so the syscall
//! layer never grows a second, subtly different copy of the authorization and
//! object-lifetime rules.

use alloc::sync::Arc;

use fs_context::FsStruct;
use kcgroup::{Cgroup, CgroupNamespace};
use kcred::{CAP_SETFCAP, CAP_SYS_ADMIN, Cred, UserNamespace};
use kerrno::LinuxError;
use kvfs::MntNamespace;

use crate::{
    IpcNamespace, NetNamespace, NsProxy, PidNamespace, TimeNamespace, UnshareError, UtsNamespace,
    types::NamespaceFlags,
};

/// Namespaces created for one `CLONE_NEW*` request.
///
/// A field is `Some` only when the matching flag was requested; `None` means
/// the operation keeps the reference already held by the parent proxy. The
/// mount namespace is paired with the retargeted filesystem context, because a
/// cloned mount tree is unusable until its root and working directory point
/// into the new tree.
pub struct NamespacePlan {
    /// Fresh user namespace owned by the caller.
    pub user_ns: Option<Arc<UserNamespace>>,
    /// Fresh cgroup view rooted at the caller's current cgroup.
    pub cgroup_ns: Option<Arc<CgroupNamespace>>,
    /// Fresh time namespace with zero offsets.
    pub time_ns: Option<Arc<TimeNamespace>>,
    /// Cloned mount namespace.
    pub mnt_ns: Option<Arc<MntNamespace>>,
    /// Fresh UTS namespace copied from the parent.
    pub uts_ns: Option<Arc<UtsNamespace>>,
    /// Fresh SysV IPC namespace.
    pub ipc_ns: Option<Arc<IpcNamespace>>,
    /// Fresh PID namespace for children.
    pub pid_ns_for_children: Option<Arc<PidNamespace>>,
    /// Fresh network namespace.
    pub net_ns: Option<Arc<NetNamespace>>,
    /// Filesystem context retargeted into [`Self::mnt_ns`].
    pub new_fs: Option<FsStruct>,
}

impl NamespacePlan {
    /// Creates the namespaces requested by `flags`.
    ///
    /// The caller's credential supplies the user namespace against which
    /// `CAP_SYS_ADMIN` is checked and the parent of a newly created user
    /// namespace. `CLONE_NEWUSER` is the one flag that does not require a
    /// capability: creating a nested user namespace is allowed for any task,
    /// and the remaining flags of the same request are authorized in that new
    /// namespace, exactly as Linux orders `create_user_ns()` before
    /// `copy_namespaces()`.
    ///
    /// `fs_context` is only consumed for `CLONE_NEWNS`; the returned plan then
    /// carries a private filesystem context whose root and working directory
    /// were retargeted into the cloned mount tree. `current_cgroup` is the
    /// caller's resolved cgroup membership and is consumed only for
    /// `CLONE_NEWCGROUP`; resolving it at the process boundary keeps `kns` free
    /// of a process-runtime dependency and avoids a global current-task lookup
    /// in the middle of namespace construction.
    pub fn build(
        flags: NamespaceFlags,
        parent: &NsProxy,
        cred: &Arc<Cred>,
        fs_context: Option<&FsStruct>,
        current_cgroup: Option<&Arc<Cgroup>>,
    ) -> Result<Self, UnshareError> {
        let needs_admin = flags.iter().any(|flag| flag != NamespaceFlags::NEWUSER);
        if needs_admin
            && !flags.contains(NamespaceFlags::NEWUSER)
            && !cred.has_cap_in(cred.user_ns(), CAP_SYS_ADMIN)
        {
            return Err(UnshareError::NotPermitted);
        }

        if flags.contains(NamespaceFlags::NEWUSER) && cred.user_ns().level() > 32 {
            return Err(UnshareError::NoSpace);
        }

        // Linux `create_user_ns()` rejects a chrooted creator: a task whose
        // fs root has moved away from the mount-namespace root must not gain
        // a fresh full capability set through `CLONE_NEWUSER`.
        if flags.contains(NamespaceFlags::NEWUSER)
            && let Some(fs_context) = fs_context
            && !fs_context
                .root()
                .ptr_eq(&parent.mnt_ns().visible_root_path())
        {
            return Err(UnshareError::NotPermitted);
        }

        // A depth-exhausted namespace tree reports ENOSPC, matching Linux
        // `create_user_ns`.
        let user_ns = if flags.contains(NamespaceFlags::NEWUSER) {
            Some(
                UserNamespace::new_child(
                    cred.user_ns_arc(),
                    cred.euid(),
                    cred.egid(),
                    cred.has_cap_in(cred.user_ns(), CAP_SETFCAP),
                )
                .map_err(|error| {
                    if LinuxError::from(error) == LinuxError::ENOSPC {
                        UnshareError::NoSpace
                    } else {
                        UnshareError::NotPermitted
                    }
                })?,
            )
        } else {
            None
        };
        // Linux owns every namespace created by one `clone`/`unshare` call in
        // the effective user namespace of the caller, which is the newly created
        // user namespace when `CLONE_NEWUSER` is present.
        let owner = user_ns
            .as_ref()
            .unwrap_or_else(|| cred.user_ns_arc())
            .clone();

        let cgroup_ns = if flags.contains(NamespaceFlags::NEWCGROUP) {
            let cgroup = current_cgroup.ok_or(UnshareError::InvalidArgument)?;
            Some(CgroupNamespace::clone_for_in(cgroup.clone(), owner.clone()))
        } else {
            None
        };

        let time_ns = flags
            .contains(NamespaceFlags::NEWTIME)
            .then(|| Arc::new(TimeNamespace::new_in(owner.clone())));

        let (mnt_ns, new_fs) = if flags.contains(NamespaceFlags::NEWNS) {
            let fs_context = fs_context.ok_or(UnshareError::InvalidArgument)?;
            let (root, pwd) = fs_context.root_and_pwd();
            let cloned = parent
                .mnt_ns()
                .clone_with_root_and_pwd_in(&root, &pwd, owner.clone())
                .map_err(UnshareError::Mount)?;
            let mut fs = fs_context.clone_for_process();
            fs.replace_root_and_pwd(cloned.root, cloned.pwd)
                .map_err(UnshareError::Mount)?;
            (Some(cloned.namespace), Some(fs))
        } else {
            (None, None)
        };

        let uts_ns = flags
            .contains(NamespaceFlags::NEWUTS)
            .then(|| Arc::new(UtsNamespace::clone_from_in(parent.uts_ns(), owner.clone())));
        let ipc_ns = flags
            .contains(NamespaceFlags::NEWIPC)
            .then(|| Arc::new(IpcNamespace::new_in(owner.clone())));
        let pid_ns_for_children = flags
            .contains(NamespaceFlags::NEWPID)
            .then(|| PidNamespace::new_child_in(parent.pid_ns_for_children(), owner.clone()));
        let net_ns = flags
            .contains(NamespaceFlags::NEWNET)
            .then(|| Arc::new(NetNamespace::new_in(owner.clone())));

        Ok(Self {
            user_ns,
            cgroup_ns,
            time_ns,
            mnt_ns,
            uts_ns,
            ipc_ns,
            pid_ns_for_children,
            net_ns,
            new_fs,
        })
    }

    /// Returns whether the plan created a namespace of any kind.
    pub fn is_empty(&self) -> bool {
        self.user_ns.is_none()
            && self.cgroup_ns.is_none()
            && self.time_ns.is_none()
            && self.mnt_ns.is_none()
            && self.uts_ns.is_none()
            && self.ipc_ns.is_none()
            && self.pid_ns_for_children.is_none()
            && self.net_ns.is_none()
    }
}
