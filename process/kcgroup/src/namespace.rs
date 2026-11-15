// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup namespace identity and view-root management.
//!
//! A cgroup namespace does not own tasks or controller state. It only pins the
//! [`Cgroup`] that appears as `/` to processes in the namespace. Switching a
//! namespace therefore changes path resolution and `cgroup.procs` authorization,
//! never task membership.

use alloc::sync::Arc;
use core::sync::atomic::{AtomicU64, Ordering};

use kcred::UserNamespace;
use klazy::Once;

use crate::{Cgroup, CpuMask};

/// Kernel-local identifier for a cgroup namespace.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub struct CgroupNamespaceId(u64);

impl Default for CgroupNamespaceId {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupNamespaceId {
    /// Allocates a new cgroup namespace identifier.
    pub fn new() -> Self {
        static NEXT_ID: AtomicU64 = AtomicU64::new(1);
        Self(NEXT_ID.fetch_add(1, Ordering::Relaxed))
    }

    /// Returns the raw identifier.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for CgroupNamespaceId {
    fn fmt(&self, formatter: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(formatter, "{}", self.0)
    }
}

static INITIAL_CGROUP_NAMESPACE: Once<Arc<CgroupNamespace>> = Once::new();

/// A cgroup namespace view rooted at one node of a hierarchy.
pub struct CgroupNamespace {
    id: CgroupNamespaceId,
    owner: Arc<UserNamespace>,
    root: Arc<Cgroup>,
}

impl Default for CgroupNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupNamespace {
    /// Creates a namespace with an independent hierarchy root.
    ///
    /// Use this only when a fresh hierarchy is intended, such as in unit tests.
    /// [`Self::clone_for`] is the `CLONE_NEWCGROUP` constructor: it roots a new
    /// view in an existing hierarchy.
    pub fn new() -> Self {
        Self::new_in(kcred::initial_user_namespace())
    }

    /// Creates a namespace with an independent hierarchy root owned by `owner`.
    pub fn new_in(owner: Arc<UserNamespace>) -> Self {
        Self {
            id: CgroupNamespaceId::new(),
            owner,
            root: Cgroup::new_root(),
        }
    }

    /// Returns the system's initial cgroup namespace.
    pub fn initial() -> Arc<Self> {
        INITIAL_CGROUP_NAMESPACE
            .call_once(|| {
                Arc::new(Self {
                    id: CgroupNamespaceId::new(),
                    owner: kcred::initial_user_namespace(),
                    root: Cgroup::new_root(),
                })
            })
            .clone()
    }

    /// Creates a namespace whose view root is `cgroup`.
    ///
    /// The new namespace shares the hierarchy and every node with the caller;
    /// only the path view changes. Dropping the namespace releases its reference
    /// to the view root, which keeps a removed node alive as a tombstone until
    /// the last namespace and mount release it.
    pub fn clone_for(cgroup: Arc<Cgroup>) -> Arc<Self> {
        Self::clone_for_in(cgroup, kcred::initial_user_namespace())
    }

    /// Creates a namespace view rooted at `cgroup`, owned by `owner`.
    pub fn clone_for_in(cgroup: Arc<Cgroup>, owner: Arc<UserNamespace>) -> Arc<Self> {
        Arc::new(Self {
            id: CgroupNamespaceId::new(),
            owner,
            root: cgroup,
        })
    }

    /// Creates a namespace view rooted at `root`.
    ///
    /// This is the owning-value form used by callers that already hold a
    /// namespace allocation; prefer [`Self::clone_for`] for `CLONE_NEWCGROUP`.
    pub fn new_view(root: Arc<Cgroup>) -> Self {
        Self::new_view_in(root, kcred::initial_user_namespace())
    }

    /// Creates a namespace view rooted at `root` and owned by `owner`.
    pub fn new_view_in(root: Arc<Cgroup>, owner: Arc<UserNamespace>) -> Self {
        Self {
            id: CgroupNamespaceId::new(),
            owner,
            root,
        }
    }

    /// Returns the namespace identity.
    pub fn id(&self) -> CgroupNamespaceId {
        self.id
    }

    /// Returns the user namespace that owns this cgroup namespace.
    pub fn owner(&self) -> &Arc<UserNamespace> {
        &self.owner
    }

    /// Returns the node that appears as `/` in this namespace.
    pub fn root(&self) -> Arc<Cgroup> {
        self.root.clone()
    }
}

/// Expands the initial root cpuset universe once the runtime CPU map is ready.
///
/// The hierarchy root is created before secondary CPUs are discovered on some
/// platforms, so its initial universe may contain only the boot CPU. Calling
/// this after CPU discovery keeps `cpuset.cpus.effective` and root affinity
/// consistent with the firmware-present CPU set.
pub fn refresh_root_cpuset_universe() {
    let root = CgroupNamespace::initial().root();
    let count = kcpu_id_map::nr_cpus();
    if count == 0 {
        return;
    }
    let mut mask = CpuMask::empty();
    for cpu in 0..count {
        mask.insert(cpu);
    }
    if mask == root.hierarchy.cpuset_universe() {
        return;
    }
    root.hierarchy.set_cpuset_universe(mask);
    let _ = root.set_cpuset_cpus(mask);
}
