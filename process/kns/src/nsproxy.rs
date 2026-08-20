// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace proxy — the namespace reference bundle for a process.

use alloc::sync::Arc;

use fs_context::FsStruct;
use kcgroup::CgroupNamespace;
use kidentity::root_pid_namespace;
use klazy::Once;
use kvfs::MntNamespace;

use crate::{
    error::CloneNsError, ipc::IpcNamespace, net::NetNamespace, pid::PidNamespace,
    time::TimeNamespace, types::NamespaceFlags, uts::UtsNamespace,
};

static INIT_NSPROXY: Once<Arc<NsProxy>> = Once::new();

/// Filesystem-context mode used while cloning namespace references.
pub enum NamespaceFsContext<'a> {
    /// The child shares `fs_struct` with the parent, so `CLONE_NEWNS` is invalid.
    Shared,
    /// The child has a private `fs_struct` that may be retargeted on `CLONE_NEWNS`.
    Private(&'a mut FsStruct),
}

/// Bundle of namespace references held by a process.
///
/// Analogous to Linux's `struct nsproxy`. Each field is an `Arc` so that
/// namespaces can be shared between processes (e.g., via `fork` without
/// `CLONE_NEW*`) or individually replaced (e.g., via `unshare` or `setns`).
/// User namespaces belong to credentials, and the active PID namespace belongs
/// to task identity rather than this bundle.
pub struct NsProxy {
    pub(crate) mnt_ns: Arc<MntNamespace>,
    pub(crate) uts_ns: Arc<UtsNamespace>,
    pub(crate) ipc_ns: Arc<IpcNamespace>,
    pub(crate) pid_ns_for_children: Arc<PidNamespace>,
    pub(crate) net_ns: Arc<NetNamespace>,
    pub(crate) cgroup_ns: Arc<CgroupNamespace>,
    pub(crate) time_ns: Arc<TimeNamespace>,
    pub(crate) time_ns_for_children: Arc<TimeNamespace>,
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

    /// Returns the cgroup namespace.
    pub fn cgroup_ns(&self) -> &Arc<CgroupNamespace> {
        &self.cgroup_ns
    }

    /// Returns the time namespace.
    pub fn time_ns(&self) -> &Arc<TimeNamespace> {
        &self.time_ns
    }

    /// Returns the time namespace used for future children.
    pub fn time_ns_for_children(&self) -> &Arc<TimeNamespace> {
        &self.time_ns_for_children
    }

    /// Creates the initial `NsProxy` used by the init process.
    pub fn new_initial() -> Arc<Self> {
        Arc::clone(INIT_NSPROXY.call_once(|| {
            let mnt_ns =
                MntNamespace::initial().expect("initial VFS mount namespace must be initialized");
            Self::new_initial_with_mnt_ns(mnt_ns)
        }))
    }

    /// Creates an initial `NsProxy` with an explicit mount namespace.
    pub fn new_initial_with_mnt_ns(mnt_ns: Arc<MntNamespace>) -> Arc<Self> {
        let root_pid_ns = root_pid_namespace().clone();
        let time_ns = Arc::new(TimeNamespace::new());
        Arc::new(Self {
            mnt_ns,
            uts_ns: Arc::new(UtsNamespace::new()),
            ipc_ns: Arc::new(IpcNamespace::new()),
            pid_ns_for_children: root_pid_ns,
            net_ns: Arc::new(NetNamespace::new()),
            cgroup_ns: Arc::new(CgroupNamespace::new()),
            time_ns: time_ns.clone(),
            time_ns_for_children: time_ns,
        })
    }

    /// Creates a child `NsProxy` based on clone flags.
    ///
    /// - Without any `CLONE_NEW*` flags, namespace references are shared.
    /// - With `CLONE_NEWNS`, a new mount namespace is cloned from the parent.
    /// - With `CLONE_NEWUTS`, a new `UtsNamespace` is cloned from the parent.
    /// - With `CLONE_NEWIPC`, a new empty `IpcNamespace` is created.
    /// - Unimplemented flags (`CLONE_NEWNET`, `CLONE_NEWUSER`, etc.) return
    ///   `Err(())` so the caller can translate to the appropriate errno.
    pub fn clone_for_child(
        &self,
        flags: NamespaceFlags,
        fs_context: NamespaceFsContext<'_>,
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

        let mnt_ns = match (flags.contains(NamespaceFlags::NEWNS), fs_context) {
            (false, _) => self.mnt_ns.clone(),
            (true, NamespaceFsContext::Shared) => {
                return Err(CloneNsError::InvalidFlagCombination);
            }
            (true, NamespaceFsContext::Private(fs)) => {
                let (root, pwd) = fs.root_and_pwd();
                let cloned = self
                    .mnt_ns
                    .clone_with_root_and_pwd(&root, &pwd)
                    .map_err(CloneNsError::Mount)?;
                fs.replace_root_and_pwd(cloned.root, cloned.pwd)
                    .map_err(CloneNsError::Mount)?;
                cloned.namespace
            }
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
            cgroup_ns: self.cgroup_ns.clone(),
            time_ns: self.time_ns.clone(),
            time_ns_for_children: self.time_ns_for_children.clone(),
        }))
    }
}

#[cfg(unittest)]
mod tests_nsproxy {
    use alloc::sync::Arc;

    use fs_context::FsStruct;
    use kcred::initial_user_namespace;
    use kvfs::{
        DirMapping, FileSystemType, FsContext, MntNamespace, Path, SimpleDir, SimpleFs, SuperBlock,
        VfsResult,
    };
    use unittest::def_test;

    use super::*;

    fn test_get_tree(
        _context: &FsContext<'_>,
        _lookup_root: &Path,
        _lookup_pwd: &Path,
    ) -> VfsResult<Arc<SuperBlock>> {
        unreachable!("the namespace test type does not provide a mount entry")
    }

    static TEST_FILE_SYSTEM_TYPE: FileSystemType = FileSystemType::nodev("kns-test", test_get_tree);

    fn make_mnt_namespace() -> Arc<MntNamespace> {
        let root_fs = SimpleFs::new_with(&TEST_FILE_SYSTEM_TYPE, 0, |fs| {
            let mut root = DirMapping::new();
            root.add_dir(
                "mnt",
                SimpleDir::new_maker(fs.clone(), Arc::new(DirMapping::new())),
            );
            SimpleDir::new_maker(fs, Arc::new(root))
        });
        MntNamespace::new_root(&root_fs, initial_user_namespace())
    }

    fn make_nsproxy() -> Arc<NsProxy> {
        NsProxy::new_initial_with_mnt_ns(make_mnt_namespace())
    }

    fn make_fs(nsproxy: &NsProxy) -> FsStruct {
        FsStruct::new(nsproxy.mnt_ns().visible_root_path())
    }

    fn clone_with_private_fs(parent: &NsProxy, flags: NamespaceFlags) -> Arc<NsProxy> {
        let mut fs = make_fs(parent);
        parent
            .clone_for_child(flags, NamespaceFsContext::Private(&mut fs))
            .unwrap()
    }

    #[def_test]
    fn test_nsproxy_initial_creation() {
        let ns = make_nsproxy();
        assert_eq!(ns.uts_ns.nodename(), b"kylin-x");
    }

    #[def_test]
    fn test_nsproxy_clone_without_flags_shares_namespaces() {
        let parent = make_nsproxy();
        let mut fs = make_fs(&parent);
        let child = parent
            .clone_for_child(
                NamespaceFlags::empty(),
                NamespaceFsContext::Private(&mut fs),
            )
            .unwrap();

        assert!(Arc::ptr_eq(&parent.uts_ns, &child.uts_ns));
        assert!(Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
        assert!(Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
    }

    #[def_test]
    fn test_nsproxy_clone_with_shared_fs_context_shares_mnt_namespace() {
        let parent = make_nsproxy();
        // CLONE_FS (shared fs_struct, no NEWNS): namespace references are still shared.
        let child = parent
            .clone_for_child(NamespaceFlags::empty(), NamespaceFsContext::Shared)
            .unwrap();

        assert!(Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
    }

    #[def_test]
    fn test_nsproxy_clone_newuts_creates_new_namespace() {
        let parent = make_nsproxy();
        parent.uts_ns.set_nodename(b"original").unwrap();

        let child = parent
            .clone_for_child(NamespaceFlags::NEWUTS, NamespaceFsContext::Shared)
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
            .clone_for_child(NamespaceFlags::NEWIPC, NamespaceFsContext::Shared)
            .unwrap();

        assert!(!Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
    }

    #[def_test]
    fn test_nsproxy_clone_newns_creates_new_mnt_namespace() {
        let parent = make_nsproxy();
        let child = clone_with_private_fs(&parent, NamespaceFlags::NEWNS);

        assert!(!Arc::ptr_eq(&parent.mnt_ns, &child.mnt_ns));
        assert!(!Arc::ptr_eq(
            parent.mnt_ns.root_mount(),
            child.mnt_ns.root_mount()
        ));
    }

    #[def_test]
    fn test_nsproxy_clone_newns_with_clone_fs_rejected() {
        let parent = make_nsproxy();
        let result = parent.clone_for_child(NamespaceFlags::NEWNS, NamespaceFsContext::Shared);
        assert!(result.is_err());
    }

    #[def_test]
    fn test_nsproxy_clone_unimplemented_flags_rejected() {
        let parent = make_nsproxy();

        assert!(
            parent
                .clone_for_child(NamespaceFlags::NEWNET, NamespaceFsContext::Shared)
                .is_err()
        );
        assert!(
            parent
                .clone_for_child(NamespaceFlags::NEWUSER, NamespaceFsContext::Shared)
                .is_err()
        );
        assert!(
            parent
                .clone_for_child(NamespaceFlags::NEWCGROUP, NamespaceFsContext::Shared)
                .is_err()
        );
        assert!(
            parent
                .clone_for_child(NamespaceFlags::NEWPID, NamespaceFsContext::Shared)
                .is_err()
        );
    }

    #[def_test]
    fn test_nsproxy_clone_preserves_shared_namespaces() {
        let parent = make_nsproxy();
        let child = parent
            .clone_for_child(NamespaceFlags::NEWUTS, NamespaceFsContext::Shared)
            .unwrap();

        // Only UTS is new; everything else is shared
        assert!(Arc::ptr_eq(&parent.ipc_ns, &child.ipc_ns));
        assert!(Arc::ptr_eq(&parent.net_ns, &child.net_ns));
        assert!(Arc::ptr_eq(
            &parent.pid_ns_for_children,
            &child.pid_ns_for_children
        ));
        assert!(Arc::ptr_eq(
            &parent.time_ns_for_children,
            &child.time_ns_for_children
        ));
    }
}
