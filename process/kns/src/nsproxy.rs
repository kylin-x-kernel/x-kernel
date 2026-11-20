// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Namespace proxy — the namespace reference bundle for a process.

use alloc::sync::Arc;

use fs_context::FsStruct;
use kcgroup::{Cgroup, CgroupNamespace};
use kcred::{Cred, UserNamespace};
use kidentity::root_pid_namespace;
use klazy::Once;
use kvfs::MntNamespace;

use crate::{
    error::CloneNsError, ipc::IpcNamespace, net::NetNamespace, nsfd::NsFd, pid::PidNamespace,
    plan::NamespacePlan, time::TimeNamespace, types::NamespaceFlags, uts::UtsNamespace,
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
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum UnshareError {
    /// The caller lacks the capability required to create or enter the
    /// requested namespace.
    NotPermitted,
    /// A Linux unshare rule rejected the flag combination (e.g. `NEWNS` while
    /// the `fs_struct` is shared with another task and `CLONE_FS` was not
    /// given) or the flag set is otherwise invalid.
    InvalidArgument,
    /// Mount-namespace copy or filesystem-context retargeting failed.
    Mount(kvfs::VfsError),
    /// A resource cap was reached (user-namespace nesting depth, ENOSPC).
    NoSpace,
}

/// Translates the shared construction error into the clone-specific domain.
fn map_clone_error(error: UnshareError) -> CloneNsError {
    match error {
        UnshareError::NotPermitted => CloneNsError::NotPermitted,
        UnshareError::InvalidArgument => CloneNsError::InvalidFlagCombination,
        UnshareError::Mount(error) => CloneNsError::Mount(error),
        UnshareError::NoSpace => CloneNsError::NoSpace,
    }
}

/// Permission and sharing facts the process layer supplies to [`unshare`].
///
/// `kns` deliberately stays free of a process-runtime dependency, so the caller
/// resolves privilege and fs-sharing state before calling
/// [`NsProxy::prepare_unshare`].
///
/// [`unshare`]: NsProxy::prepare_unshare
#[derive(Clone)]
pub struct UnshareContext {
    /// Caller credential: authorization source and parent of a new user
    /// namespace.
    pub cred: Arc<Cred>,
    /// Whether the current `fs_struct` is shared with another task (a thread of
    /// the same process or another process forked with `CLONE_FS`).
    pub fs_shared_with_others: bool,
    /// Whether the caller also requested `CLONE_FS`.
    pub unshare_fs: bool,
    /// Whether the caller also requested `CLONE_FILES`.
    pub unshare_files: bool,
    /// Caller's current cgroup membership, resolved by the process layer for
    /// `CLONE_NEWCGROUP`.
    pub current_cgroup: Option<Arc<Cgroup>>,
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
    /// User namespace to install as the caller's credential namespace.
    pub(crate) user_ns: Option<Arc<UserNamespace>>,
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

    /// Returns the user namespace to install in the caller's credential.
    pub fn user_ns(&self) -> Option<&alloc::sync::Arc<UserNamespace>> {
        self.user_ns.as_ref()
    }

    /// Returns whether the commit must also unshare the file-descriptor table.
    pub fn unshare_files(&self) -> bool {
        self.unshare_files
    }

    /// Consumes the bundle into the state the process layer installs.
    ///
    /// The prepared filesystem context is already private
    /// ([`FsStruct::clone_for_process`]), so the commit must install it
    /// directly instead of snapshotting it again: a snapshot would copy the
    /// inherited `in_exec` marker and leave the new context permanently
    /// unavailable for `CLONE_FS`.
    #[doc(hidden)]
    pub fn into_parts(self) -> (Arc<NsProxy>, Option<FsStruct>, bool) {
        (self.nsproxy, self.new_fs, self.unshare_files)
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
        let time_ns = Arc::new(TimeNamespace::initial());
        Arc::new(Self {
            mnt_ns,
            uts_ns: Arc::new(UtsNamespace::new()),
            ipc_ns: Arc::new(IpcNamespace::new()),
            pid_ns_for_children: root_pid_ns,
            net_ns: Arc::new(NetNamespace::new_initial()),
            cgroup_ns: CgroupNamespace::initial(),
            time_ns: time_ns.clone(),
            time_ns_for_children: time_ns,
        })
    }

    /// Materializes the namespaces selected by `flags` for a child process.
    ///
    /// The child receives the same references as its parent unless the matching
    /// `CLONE_NEW*` flag asks for a fresh object. User namespaces live in
    /// credentials rather than [`NsProxy`], so `CLONE_NEWUSER` is reported to
    /// the caller through [`NamespacePlan::user_ns`] and committed with the
    /// child credential by the process layer.
    ///
    /// The `cred` argument authorizes namespace creation and becomes the
    /// parent of a new user namespace. `CLONE_NEWUSER` alone needs no
    /// capability; every other `CLONE_NEW*` flag requires `CAP_SYS_ADMIN` in
    /// `cred`'s user namespace.
    ///
    /// # Errors
    ///
    /// [`CloneNsError::NotPermitted`] when authorization fails,
    /// [`CloneNsError::InvalidFlagCombination`] for `CLONE_NEWNS` with a
    /// shared filesystem context, and [`CloneNsError::Mount`] when the mount
    /// tree copy or retargeting fails.
    pub fn clone_for_child(
        &self,
        flags: NamespaceFlags,
        mut fs_context: NamespaceFsContext<'_>,
        cred: &Arc<Cred>,
        current_cgroup: Option<&Arc<Cgroup>>,
    ) -> Result<(Arc<Self>, Option<Arc<UserNamespace>>), CloneNsError> {
        let current_fs = match &fs_context {
            NamespaceFsContext::Shared => None,
            NamespaceFsContext::Private(fs) => Some(&**fs),
        };
        let plan = NamespacePlan::build(flags, self, cred, current_fs, current_cgroup)
            .map_err(map_clone_error)?;

        if let (NamespaceFsContext::Private(fs), Some(planned_fs)) =
            (&mut fs_context, plan.new_fs.as_ref())
        {
            let (root, pwd) = planned_fs.root_and_pwd();
            fs.replace_root_and_pwd(root, pwd)
                .map_err(CloneNsError::Mount)?;
        }

        Ok((self.instantiate(&plan, true), plan.user_ns))
    }

    /// Two-phase `unshare(2)` preparation: validates and builds the replacement
    /// namespace bundle without touching the running process.
    ///
    /// Every Linux rule that can fail is checked here so the commit phase only
    /// installs precomputed state:
    ///
    /// - `NEWUSER` alone needs no capability; every other `NEW*` flag requires
    ///   `CAP_SYS_ADMIN` in [`UnshareContext::cred`]'s user namespace.
    /// - `NEWNS` while the `fs_struct` is shared with another task and
    ///   `CLONE_FS` was not also requested is `EINVAL`, matching Linux
    ///   `check_fs_flag()`.
    /// - `NEWNS` clones the mount tree at the current root/pwd and builds a
    ///   private `FsStruct` retargeted into the clone.
    /// - `NEWTIME` creates a zero-offset namespace that becomes
    ///   `time_ns_for_children`; the caller keeps its active time namespace
    ///   until it forks, matching Linux `unshare(CLONE_NEWTIME)`.
    ///
    /// `CLONE_FS` without `NEWNS` still detaches a private `FsStruct` copy;
    /// `CLONE_FILES` is recorded in [`PreparedUnshare::unshare_files`] and
    /// applied by the commit phase.
    ///
    /// # Errors
    ///
    /// - [`UnshareError::NotPermitted`] — a namespace other than `NEWUSER` was
    ///   requested without `CAP_SYS_ADMIN` in the caller's user namespace.
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
    /// use kcred::Cred;
    /// use kns::{NamespaceFlags, NsProxy, UnshareContext};
    ///
    /// let proxy = NsProxy::new_initial();
    /// let fs = FsStruct::new(proxy.mnt_ns().visible_root_path());
    /// let context = UnshareContext {
    ///     cred: alloc::sync::Arc::new(Cred::root()),
    ///     fs_shared_with_others: false,
    ///     unshare_fs: false,
    ///     unshare_files: true,
    ///     current_cgroup: None,
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
        if flags.contains(NamespaceFlags::NEWNS)
            && context.fs_shared_with_others
            && !context.unshare_fs
        {
            return Err(UnshareError::InvalidArgument);
        }

        let plan = NamespacePlan::build(
            flags,
            self,
            &context.cred,
            Some(fs_context),
            context.current_cgroup.as_ref(),
        )?;
        let nsproxy = self.instantiate(&plan, false);
        let new_fs = plan
            .new_fs
            .or_else(|| context.unshare_fs.then(|| fs_context.clone_for_process()));

        Ok(PreparedUnshare {
            nsproxy,
            new_fs,
            unshare_files: context.unshare_files,
            user_ns: plan.user_ns,
        })
    }

    /// Builds a proxy with `plan`'s new references swapped into their slots.
    ///
    /// `replace_active_time` distinguishes the two callers: a cloned child
    /// enters the new time namespace immediately, while `unshare(CLONE_NEWTIME)`
    /// only changes `time_ns_for_children` and leaves the caller's active time
    /// namespace in place until it forks.
    fn instantiate(&self, plan: &NamespacePlan, replace_active_time: bool) -> Arc<Self> {
        let child_time_ns = plan.time_ns.clone();
        Arc::new(Self {
            mnt_ns: plan.mnt_ns.clone().unwrap_or_else(|| self.mnt_ns.clone()),
            uts_ns: plan.uts_ns.clone().unwrap_or_else(|| self.uts_ns.clone()),
            ipc_ns: plan.ipc_ns.clone().unwrap_or_else(|| self.ipc_ns.clone()),
            pid_ns_for_children: plan
                .pid_ns_for_children
                .clone()
                .unwrap_or_else(|| self.pid_ns_for_children.clone()),
            net_ns: plan.net_ns.clone().unwrap_or_else(|| self.net_ns.clone()),
            cgroup_ns: plan
                .cgroup_ns
                .clone()
                .unwrap_or_else(|| self.cgroup_ns.clone()),
            time_ns: if replace_active_time {
                // A new child enters the time namespace selected for children,
                // which differs from the caller's active namespace after
                // unshare(CLONE_NEWTIME).
                child_time_ns
                    .clone()
                    .unwrap_or_else(|| self.time_ns_for_children.clone())
            } else {
                self.time_ns.clone()
            },
            time_ns_for_children: child_time_ns
                .unwrap_or_else(|| self.time_ns_for_children.clone()),
        })
    }

    /// Builds a proxy with the namespace pinned by `replacement` swapped into
    /// the matching slot, used by `setns(2)`.
    ///
    /// Only the namespace field whose type matches [`NsFd::ns_type`] is
    /// replaced; every other reference is shared with the current proxy.
    ///
    /// `Time` replaces both [`NsProxy::time_ns`] and
    /// [`NsProxy::time_ns_for_children`], matching Linux `timens_install()`.
    /// Unlike `unshare(CLONE_NEWTIME)`, `setns(CLONE_NEWTIME)` moves the caller
    /// into the target namespace as well as selecting it for future children.
    ///
    /// The user namespace is owned by credentials rather than by this bundle,
    /// so this method leaves every field here unchanged for it; the syscall
    /// layer installs the matching credential transition together with this
    /// proxy.
    pub fn with_namespace(&self, replacement: &NsFd) -> Result<Arc<Self>, UnshareError> {
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
        let net_ns = match replacement {
            NsFd::Net(ns) => ns.clone(),
            _ => self.net_ns.clone(),
        };
        let cgroup_ns = match replacement {
            NsFd::Cgroup(ns) => ns.clone(),
            _ => self.cgroup_ns.clone(),
        };
        let (time_ns, time_ns_for_children) = match replacement {
            NsFd::Time(ns) => (ns.clone(), ns.clone()),
            _ => (self.time_ns.clone(), self.time_ns_for_children.clone()),
        };

        Ok(Arc::new(Self {
            mnt_ns,
            uts_ns,
            ipc_ns,
            pid_ns_for_children,
            net_ns,
            cgroup_ns,
            time_ns,
            time_ns_for_children,
        }))
    }
}

unittest::path_mod_test!(tests_nsproxy, "../ut-tests/nsproxy.rs");
