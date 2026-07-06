// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace proxy — the namespace reference bundle for a process.

use alloc::sync::Arc;

use kcgroup::CgroupNamespace;
use kfs::FsContext;
use ksync::Mutex;

use crate::{
    error::CloneNsError, ipc::IpcNamespace, mnt::MntNamespace, net::NetNamespace,
    pid::PidNamespace, time::TimeNamespace, types::NamespaceFlags, user::UserNamespace,
    uts::UtsNamespace,
};

/// Bundle of namespace references held by a process.
///
/// Analogous to Linux's `struct nsproxy`. Each field is an `Arc` so that
/// namespaces can be shared between processes (e.g., via `fork` without
/// `CLONE_NEW*`) or individually replaced (e.g., via `unshare` or `setns`).
pub struct NsProxy {
    pub(crate) mnt_ns: Arc<MntNamespace>,
    pub(crate) uts_ns: Arc<UtsNamespace>,
    pub(crate) ipc_ns: Arc<IpcNamespace>,
    pub(crate) pid_ns_for_children: Arc<PidNamespace>,
    pub(crate) net_ns: Arc<NetNamespace>,
    pub(crate) user_ns: Arc<UserNamespace>,
    pub(crate) cgroup_ns: Arc<CgroupNamespace>,
    pub(crate) time_ns: Arc<TimeNamespace>,
}

impl NsProxy {
    /// Returns the mount namespace.
    pub fn mnt_ns(&self) -> &Arc<MntNamespace> {
        &self.mnt_ns
    }

    /// Returns the UTS namespace.
    pub fn uts_ns(&self) -> &Arc<UtsNamespace> {
        &self.uts_ns
    }

    /// Returns the IPC namespace.
    pub fn ipc_ns(&self) -> &Arc<IpcNamespace> {
        &self.ipc_ns
    }

    /// Returns the PID namespace used for future children.
    pub fn pid_ns_for_children(&self) -> &Arc<PidNamespace> {
        &self.pid_ns_for_children
    }

    /// Returns the network namespace.
    pub fn net_ns(&self) -> &Arc<NetNamespace> {
        &self.net_ns
    }

    /// Returns the user namespace.
    pub fn user_ns(&self) -> &Arc<UserNamespace> {
        &self.user_ns
    }

    /// Returns the cgroup namespace.
    pub fn cgroup_ns(&self) -> &Arc<CgroupNamespace> {
        &self.cgroup_ns
    }

    /// Returns the time namespace.
    pub fn time_ns(&self) -> &Arc<TimeNamespace> {
        &self.time_ns
    }

    /// Creates the initial `NsProxy` used by the init process.
    pub fn new_initial(fs_context: Arc<Mutex<FsContext>>) -> Arc<Self> {
        Arc::new(Self {
            mnt_ns: Arc::new(MntNamespace::new(fs_context)),
            uts_ns: Arc::new(UtsNamespace::new()),
            ipc_ns: Arc::new(IpcNamespace::new()),
            pid_ns_for_children: Arc::new(PidNamespace::new_root()),
            net_ns: Arc::new(NetNamespace::new()),
            user_ns: Arc::new(UserNamespace::new_root()),
            cgroup_ns: Arc::new(CgroupNamespace::new()),
            time_ns: Arc::new(TimeNamespace::new()),
        })
    }

    /// Creates a child `NsProxy` based on clone flags.
    ///
    /// - Without any `CLONE_NEW*` flags, namespaces are shared (Arc-cloned).
    ///   The mount namespace is shared only when `share_fs` is true
    ///   (`CLONE_FS`); otherwise the child gets a *new* `MntNamespace` wrapping
    ///   a clone of the parent's `FsContext`, so the child has a private
    ///   cwd/root while still sharing the underlying mount tree (Phase 1
    ///   semantics).
    /// - With `CLONE_NEWNS`, a new `MntNamespace` is created from a clone of
    ///   the parent's `FsContext`. `CLONE_NEWNS | CLONE_FS` is rejected.
    /// - With `CLONE_NEWUTS`, a new `UtsNamespace` is cloned from the parent.
    /// - With `CLONE_NEWIPC`, a new empty `IpcNamespace` is created.
    /// - Unimplemented flags (`CLONE_NEWNET`, `CLONE_NEWUSER`, etc.) return
    ///   `Err(())` so the caller can translate to the appropriate errno.
    pub fn clone_for_child(
        &self,
        flags: NamespaceFlags,
        share_fs: bool,
    ) -> Result<Arc<Self>, CloneNsError> {
        // Reject unimplemented namespace flags
        let unimplemented = NamespaceFlags::NEWNET
            | NamespaceFlags::NEWUSER
            | NamespaceFlags::NEWCGROUP
            | NamespaceFlags::NEWPID
            | NamespaceFlags::NEWTIME;
        if flags.intersects(unimplemented) {
            return Err(CloneNsError::Unimplemented);
        }

        // CLONE_NEWNS and CLONE_FS are mutually exclusive
        if flags.contains(NamespaceFlags::NEWNS) && share_fs {
            return Err(CloneNsError::InvalidFlagCombination);
        }

        // The child's mount namespace / FsContext is determined by:
        //  - CLONE_NEWNS: a brand-new MntNamespace around a cloned FsContext.
        //  - CLONE_FS (share_fs, no NEWNS): share the *same* MntNamespace Arc,
        //    so cwd/root mutations are visible to both parent and child.
        //  - otherwise (plain fork): a new MntNamespace wrapping a cloned
        //    FsContext, giving the child a private cwd/root (Linux semantics).
        let mnt_ns = if flags.contains(NamespaceFlags::NEWNS) {
            let cloned_fs = {
                let fs = self.mnt_ns.fs_context().lock();
                fs.clone()
            };
            Arc::new(MntNamespace::new(Arc::new(Mutex::new(cloned_fs))))
        } else if share_fs {
            self.mnt_ns.clone()
        } else {
            let cloned_fs = {
                let fs = self.mnt_ns.fs_context().lock();
                fs.clone()
            };
            Arc::new(MntNamespace::new(Arc::new(Mutex::new(cloned_fs))))
        };

        let uts_ns = if flags.contains(NamespaceFlags::NEWUTS) {
            Arc::new(UtsNamespace::clone_from(&self.uts_ns))
        } else {
            self.uts_ns.clone()
        };

        let ipc_ns = if flags.contains(NamespaceFlags::NEWIPC) {
            Arc::new(IpcNamespace::new())
        } else {
            self.ipc_ns.clone()
        };

        Ok(Arc::new(Self {
            mnt_ns,
            uts_ns,
            ipc_ns,
            pid_ns_for_children: self.pid_ns_for_children.clone(),
            net_ns: self.net_ns.clone(),
            user_ns: self.user_ns.clone(),
            cgroup_ns: self.cgroup_ns.clone(),
            time_ns: self.time_ns.clone(),
        }))
    }
}

#[cfg(unittest)]
mod tests_nsproxy {
    use alloc::sync::Arc;

    use kfs::new_process_fs_context;
    use unittest::def_test;

    use super::*;

    fn make_nsproxy() -> Arc<NsProxy> {
        NsProxy::new_initial(new_process_fs_context())
    }

    #[def_test]
    fn test_nsproxy_initial_creation() {
        let ns = make_nsproxy();
        assert_eq!(ns.uts_ns.nodename(), b"kylin-x");
    }

    #[def_test]
    fn test_nsproxy_clone_without_flags_shares_namespaces() {
        let parent = make_nsproxy();
        // Plain fork (no CLONE_FS): child gets a private mnt namespace wrapping
        // a cloned FsContext, but shares all other namespaces.
        let child = parent
            .clone_for_child(NamespaceFlags::empty(), false)
            .unwrap();

        assert!(Arc::ptr_eq(&parent.uts_ns, &child.uts_ns));
        assert!(Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
        assert!(!Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
    }

    #[def_test]
    fn test_nsproxy_clone_with_share_fs_shares_mnt_namespace() {
        let parent = make_nsproxy();
        // CLONE_FS (share_fs, no NEWNS): the same MntNamespace Arc is shared,
        // so cwd/root mutations are visible to both parent and child.
        let child = parent
            .clone_for_child(NamespaceFlags::empty(), true)
            .unwrap();

        assert!(Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
    }

    #[def_test]
    fn test_nsproxy_clone_newuts_creates_new_namespace() {
        let parent = make_nsproxy();
        parent.uts_ns.set_nodename(b"original").unwrap();

        let child = parent
            .clone_for_child(NamespaceFlags::NEWUTS, false)
            .unwrap();

        // Child gets a new UTS namespace with copied values
        assert!(!Arc::ptr_eq(&parent.uts_ns, &child.uts_ns));
        assert_eq!(child.uts_ns.nodename(), b"original");

        // Modifying child does not affect parent
        child.uts_ns.set_nodename(b"modified").unwrap();
        assert_eq!(parent.uts_ns.nodename(), b"original");
        assert_eq!(child.uts_ns.nodename(), b"modified");
    }

    #[def_test]
    fn test_nsproxy_clone_newipc_creates_new_namespace() {
        let parent = make_nsproxy();
        let child = parent
            .clone_for_child(NamespaceFlags::NEWIPC, false)
            .unwrap();

        assert!(!Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
    }

    #[def_test]
    fn test_nsproxy_clone_newns_creates_new_mnt_namespace() {
        let parent = make_nsproxy();
        let child = parent
            .clone_for_child(NamespaceFlags::NEWNS, false)
            .unwrap();

        assert!(!Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
    }

    #[def_test]
    fn test_nsproxy_clone_newns_with_clone_fs_rejected() {
        let parent = make_nsproxy();
        let result = parent.clone_for_child(NamespaceFlags::NEWNS, true);
        assert!(result.is_err());
    }

    #[def_test]
    fn test_nsproxy_clone_unimplemented_flags_rejected() {
        let parent = make_nsproxy();

        assert!(
            parent
                .clone_for_child(NamespaceFlags::NEWNET, false)
                .is_err()
        );
        assert!(
            parent
                .clone_for_child(NamespaceFlags::NEWUSER, false)
                .is_err()
        );
        assert!(
            parent
                .clone_for_child(NamespaceFlags::NEWCGROUP, false)
                .is_err()
        );
        assert!(
            parent
                .clone_for_child(NamespaceFlags::NEWPID, false)
                .is_err()
        );
    }

    #[def_test]
    fn test_nsproxy_clone_preserves_shared_namespaces() {
        let parent = make_nsproxy();
        let child = parent
            .clone_for_child(NamespaceFlags::NEWUTS, false)
            .unwrap();

        // Only UTS is new; everything else is shared
        assert!(Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
        assert!(Arc::ptr_eq(&parent.net_ns, &child.net_ns));
        assert!(Arc::ptr_eq(&parent.user_ns, &child.user_ns));
        assert!(Arc::ptr_eq(
            &parent.pid_ns_for_children,
            &child.pid_ns_for_children
        ));
    }
}
