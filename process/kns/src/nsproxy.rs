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
    error::CloneNsError, ipc::IpcNamespace, net::NetNamespace, nsfd::NsFd, pid::PidNamespace,
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
/// namespaces can be shared between processes through `fork` without
/// `CLONE_NEW*`, or selected references can be copied during child creation.
/// This crate does not implement runtime `unshare` or `setns` replacement.
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

/// Errors reported by [`NsProxy::prepare_unshare`] and [`NsProxy::with_namespace`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UnshareError {
    /// A namespace creation flag was requested without the required
    /// capability (`CAP_SYS_ADMIN` in the absence of user namespaces).
    NotPermitted,
    /// A Linux unshare rule rejected the flag combination (e.g. `NEWNS` while
    /// the `fs_struct` is shared with another task and `CLONE_FS` was not
    /// given) or the flag set is otherwise invalid.
    InvalidArgument,
    /// The requested namespace kind is not implemented yet and must not fall
    /// back to pseudo-isolation (`NEWNET`, `NEWUSER`, `NEWCGROUP`, `NEWTIME`).
    Unsupported,
    /// Mount-namespace copy or filesystem-context retargeting failed.
    Mount(kvfs::VfsError),
}

/// Permission and sharing facts the process layer supplies to [`unshare`].
///
/// `kns` deliberately stays free of a process-runtime dependency, so the caller
/// resolves privilege and fs-sharing state before calling
/// [`NsProxy::prepare_unshare`].
///
/// [`unshare`]: NsProxy::prepare_unshare
#[derive(Clone, Copy, Debug)]
pub struct UnshareContext {
    /// Whether the caller holds `CAP_SYS_ADMIN`. X-Kernel currently uses
    /// effective-uid-zero as the capability proxy (see `kcred::Cred::is_privileged`).
    pub privileged: bool,
    /// Whether the current `fs_struct` is shared with another task (a thread of
    /// the same process or another process forked with `CLONE_FS`).
    pub fs_shared_with_others: bool,
    /// Whether the caller also requested `CLONE_FS`.
    pub unshare_fs: bool,
    /// Whether the caller also requested `CLONE_FILES`.
    pub unshare_files: bool,
}

/// Validated, ready-to-commit unshare result produced by
/// [`NsProxy::prepare_unshare`].
///
/// Construction performs every Linux acl/combination check; the process layer
/// commits the bundle by replacing its stored namespace proxy (and filesystem
/// context when [`Self::new_fs`] is present) and, when requested, unsharing the
/// file-descriptor table.
pub struct PreparedUnshare {
    /// Namespace bundle with the requested references copied.
    pub(crate) nsproxy: Arc<NsProxy>,
    /// Private filesystem context to install when `NEWNS` or `CLONE_FS` was
    /// requested. `None` when the process keeps its current context.
    pub(crate) new_fs: Option<FsStruct>,
    /// Whether the file-descriptor table must be unshared on commit.
    pub(crate) unshare_files: bool,
}

impl PreparedUnshare {
    /// Returns the namespace bundle to commit.
    pub fn nsproxy(&self) -> &Arc<NsProxy> {
        &self.nsproxy
    }

    /// Returns the private filesystem context to install, if one was prepared.
    pub fn new_fs(&self) -> Option<&FsStruct> {
        self.new_fs.as_ref()
    }

    /// Returns whether the commit must also unshare the file-descriptor table.
    pub fn unshare_files(&self) -> bool {
        self.unshare_files
    }
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
    ///
    /// # Panics
    ///
    /// Panics if the initial VFS mount namespace has not been installed.
    /// Boot must initialize VFS before calling this entry point.
    pub fn new_initial() -> Arc<Self> {
        Arc::clone(INIT_NSPROXY.call_once(|| {
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: VFS boot initializes the \
                          permanent mount namespace before initial namespace-proxy creation"
            )]
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
            cgroup_ns: CgroupNamespace::initial(),
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
    /// - With `CLONE_NEWPID`, a child PID namespace is created below the parent
    ///   and installed as `pid_ns_for_children`; the child task identity is
    ///   allocated in it by the process layer.
    /// - Unimplemented flags (`CLONE_NEWCGROUP`, `CLONE_NEWNET`, etc.) return an
    ///   error so the caller can translate it to the appropriate errno.
    ///
    /// # Errors
    ///
    /// Returns `CloneNsError::Unimplemented` for NEWNET, NEWUSER, NEWCGROUP,
    /// or NEWTIME — their backing tables are not isolated, so cloning the
    /// reference would create pseudo-isolation. Returns
    /// `CloneNsError::InvalidFlagCombination` for NEWNS with a shared
    /// filesystem context. Mount cloning or root/pwd retarget errors are
    /// wrapped in `CloneNsError::Mount`.
    ///
    /// # Panics
    ///
    /// With NEWNS, panics if the private context has no initialized root or pwd.
    /// The caller must prepare a mounted private context before cloning.
    ///
    /// # Example
    ///
    /// After VFS boot initialization, clone the mount namespace using a private,
    /// already-mounted filesystem context:
    ///
    /// ```no_run
    /// extern crate alloc;
    /// use fs_context::FsStruct;
    /// use kns::{NamespaceFlags, NamespaceFsContext, NsProxy};
    /// let parent = NsProxy::new_initial();
    /// let mut fs = FsStruct::new(parent.mnt_ns().visible_root_path());
    /// let child = parent
    ///     .clone_for_child(NamespaceFlags::NEWNS, NamespaceFsContext::Private(&mut fs))
    ///     .unwrap();
    /// assert!(!alloc::sync::Arc::ptr_eq(parent.mnt_ns(), child.mnt_ns()));
    /// ```
    pub fn clone_for_child(
        &self,
        flags: NamespaceFlags,
        fs_context: NamespaceFsContext<'_>,
    ) -> Result<Arc<Self>, CloneNsError> {
        // Reject unimplemented namespace flags. Their backing tables are not
        // isolated, so cloning the reference would create pseudo-isolation.
        let unimplemented = NamespaceFlags::NEWNET
            | NamespaceFlags::NEWUSER
            | NamespaceFlags::NEWCGROUP
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

        let pid_ns_for_children = if flags.contains(NamespaceFlags::NEWPID) {
            PidNamespace::new_child(&self.pid_ns_for_children)
        } else {
            self.pid_ns_for_children.clone()
        };

        Ok(Arc::new(Self {
            mnt_ns,
            uts_ns,
            ipc_ns,
            pid_ns_for_children,
            // `clone` of the remaining flags shares the network namespace: it
            // has no isolated backing table, so a fresh identity alone would
            // be pseudo-isolation (see `clone_for_child`'s rejection above).
            net_ns: self.net_ns.clone(),
            cgroup_ns: self.cgroup_ns.clone(),
            time_ns: self.time_ns.clone(),
            time_ns_for_children: self.time_ns_for_children.clone(),
        }))
    }

    /// Two-phase `unshare(2)` preparation: validates and builds the replacement
    /// namespace bundle without touching the running process.
    ///
    /// Every Linux rule that can fail is checked here so the commit phase only
    /// installs precomputed state:
    ///
    /// - `NEWNET`, `NEWUSER`, `NEWCGROUP`, and `NEWTIME` are rejected with
    ///   [`UnshareError::Unsupported`]: creating namespace objects that only
    ///   change an ID without isolating the underlying tables would be
    ///   pseudo-isolation. `knet` still resolves sockets through its global
    ///   protocol stack, so a network namespace without a per-namespace data
    ///   plane stays unsupported.
    /// - Any `NEW*` flag requires [`UnshareContext::privileged`]
    ///   (`CAP_SYS_ADMIN` in the absence of user namespaces).
    /// - `NEWNS` while the `fs_struct` is shared with another task and
    ///   `CLONE_FS` was not also requested is `EINVAL`, matching Linux
    ///   `check_fs_flag()`.
    /// - `NEWNS` clones the mount tree at the current root/pwd and builds a
    ///   private `FsStruct` retargeted into the clone.
    ///
    /// `CLONE_FS` without `NEWNS` still detaches a private `FsStruct` copy;
    /// `CLONE_FILES` is recorded in [`PreparedUnshare::unshare_files`] and
    /// applied by the commit phase.
    ///
    /// # Errors
    ///
    /// - [`UnshareError::Unsupported`] — any of `NEWNET`, `NEWUSER`,
    ///   `NEWCGROUP`, or `NEWTIME` was requested.
    /// - [`UnshareError::NotPermitted`] — a `NEW*` flag was requested while
    ///   [`UnshareContext::privileged`] is false.
    /// - [`UnshareError::InvalidArgument`] — `NEWNS` was requested while
    ///   [`UnshareContext::fs_shared_with_others`] holds and `CLONE_FS`
    ///   (`unshare_fs`) was not also requested.
    /// - [`UnshareError::Mount`] — the mount-tree copy or the root/pwd
    ///   retarget into the clone failed; the wrapped source is the underlying
    ///   VFS error.
    ///
    /// # Examples
    ///
    /// Build the context from the caller's credential and fs-sharing state,
    /// then prepare against the caller's current fs view; the returned
    /// [`PreparedUnshare`] is installed later by the process-layer commit:
    ///
    /// ```no_run
    /// use fs_context::FsStruct;
    /// use kns::{NamespaceFlags, NsProxy, UnshareContext};
    ///
    /// let proxy = NsProxy::new_initial();
    /// let fs = FsStruct::new(proxy.mnt_ns().visible_root_path());
    /// let context = UnshareContext {
    ///     privileged: true,
    ///     fs_shared_with_others: false,
    ///     unshare_fs: false,
    ///     unshare_files: true,
    /// };
    /// let _prepared = proxy
    ///     .prepare_unshare(NamespaceFlags::NEWUTS, context, &fs)
    ///     .unwrap();
    /// ```
    pub fn prepare_unshare(
        &self,
        flags: NamespaceFlags,
        context: UnshareContext,
        fs_context: &FsStruct,
    ) -> Result<PreparedUnshare, UnshareError> {
        let unsupported = NamespaceFlags::NEWNET
            | NamespaceFlags::NEWUSER
            | NamespaceFlags::NEWCGROUP
            | NamespaceFlags::NEWTIME;
        if flags.intersects(unsupported) {
            return Err(UnshareError::Unsupported);
        }

        if !flags.is_empty() && !context.privileged {
            return Err(UnshareError::NotPermitted);
        }

        if flags.contains(NamespaceFlags::NEWNS)
            && context.fs_shared_with_others
            && !context.unshare_fs
        {
            return Err(UnshareError::InvalidArgument);
        }

        let (mnt_ns, new_fs) = if flags.contains(NamespaceFlags::NEWNS) {
            let (root, pwd) = fs_context.root_and_pwd();
            let cloned = self
                .mnt_ns
                .clone_with_root_and_pwd(&root, &pwd)
                .map_err(UnshareError::Mount)?;
            let mut fs = fs_context.clone_for_process();
            fs.replace_root_and_pwd(cloned.root, cloned.pwd)
                .map_err(UnshareError::Mount)?;
            (cloned.namespace, Some(fs))
        } else {
            (self.mnt_ns.clone(), None)
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

        let pid_ns_for_children = if flags.contains(NamespaceFlags::NEWPID) {
            PidNamespace::new_child(&self.pid_ns_for_children)
        } else {
            self.pid_ns_for_children.clone()
        };

        // `unshare` of the remaining flags shares the network namespace: it
        // has no isolated backing table, so a fresh identity alone would be
        // pseudo-isolation (see `prepare_unshare`'s rejection above).
        let net_ns = self.net_ns.clone();

        let nsproxy = Arc::new(Self {
            mnt_ns,
            uts_ns,
            ipc_ns,
            pid_ns_for_children,
            net_ns,
            cgroup_ns: self.cgroup_ns.clone(),
            time_ns: self.time_ns.clone(),
            time_ns_for_children: self.time_ns_for_children.clone(),
        });

        let new_fs = new_fs.or_else(|| context.unshare_fs.then(|| fs_context.clone_for_process()));

        Ok(PreparedUnshare {
            nsproxy,
            new_fs,
            unshare_files: context.unshare_files,
        })
    }

    /// Builds a proxy with the namespace pinned by `replacement` swapped into
    /// the matching slot, used by `setns(2)`.
    ///
    /// Only the namespace field whose type matches [`NsFd::ns_type`] is
    /// replaced; every other reference is shared with the current proxy.
    ///
    /// # Errors
    ///
    /// Returns [`UnshareError::Unsupported`] for `User`, `Cgroup`, and `Net`
    /// namespaces, whose underlying tables are not isolated yet; switching
    /// only the namespace reference would create pseudo-isolation.
    pub fn with_namespace(&self, replacement: &NsFd) -> Result<Arc<Self>, UnshareError> {
        if matches!(replacement, NsFd::User(_) | NsFd::Cgroup(_) | NsFd::Net(_)) {
            return Err(UnshareError::Unsupported);
        }

        let mnt_ns = match replacement {
            NsFd::Mnt(ns) => ns.clone(),
            _ => self.mnt_ns.clone(),
        };
        let uts_ns = match replacement {
            NsFd::Uts(ns) => ns.clone(),
            _ => self.uts_ns.clone(),
        };
        let ipc_ns = match replacement {
            NsFd::Ipc(ns) => ns.clone(),
            _ => self.ipc_ns.clone(),
        };
        let pid_ns_for_children = match replacement {
            NsFd::Pid(ns) => ns.clone(),
            _ => self.pid_ns_for_children.clone(),
        };
        let net_ns = self.net_ns.clone();

        Ok(Arc::new(Self {
            mnt_ns,
            uts_ns,
            ipc_ns,
            pid_ns_for_children,
            net_ns,
            cgroup_ns: self.cgroup_ns.clone(),
            time_ns: self.time_ns.clone(),
            time_ns_for_children: self.time_ns_for_children.clone(),
        }))
    }
}

unittest::path_mod_test!(tests_nsproxy, "../ut-tests/nsproxy.rs");
