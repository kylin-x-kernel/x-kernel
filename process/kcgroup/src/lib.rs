// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup v2 hierarchy, pids admission, and stable task membership.
//!
//! Use [`CgroupNamespace::initial`] for the system hierarchy. Adapters must supply
//! authorization; they must not keep parallel membership/controller state.
//! Reserve a [`TaskCharge`] before task publication and commit it to a stable
//! identity, or drop it to roll back. APIs require kernel allocation and sleepable
//! locks; migration intentionally ignores new-task pids limits.
//!
//! # Example
//!
//! ```no_run
//! use kcgroup::CgroupNamespace;
//! let root = CgroupNamespace::new().root();
//! root.set_pids_subtree_enabled(true).unwrap();
//! let group = root.create_child("worker").unwrap();
//! let operation = group.begin_operation().unwrap();
//! group.set_pids_max(Some(1)).unwrap();
//! drop(operation);
//! let identity = kidentity::allocate_root_pid_handle().unwrap();
//! let member = group.reserve_task().unwrap().commit(identity).unwrap();
//! assert_eq!(group.pids_current(), Ok(1));
//! member.detach();
//! root.remove_child("worker").unwrap();
//! ```

#![no_std]
#![deny(unsafe_code)]

extern crate alloc;

use alloc::{
    collections::BTreeMap,
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};

use kerrno::{KError, KResult, LinuxError};
use klazy::Once;
use ksync::{Mutex, RwLock};

static INITIAL_CGROUP_NAMESPACE: Once<Arc<CgroupNamespace>> = Once::new();

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

    /// Returns the raw identifier value.
    pub fn as_u64(self) -> u64 {
        self.0
    }
}

impl core::fmt::Display for CgroupNamespaceId {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A node in the system cgroup v2 hierarchy.
pub struct Cgroup {
    name: String,
    parent: Weak<Cgroup>,
    hierarchy: Arc<CgroupHierarchy>,
    children: RwLock<BTreeMap<String, Arc<Cgroup>>>,
    pids: RwLock<Option<Arc<PidsController>>>,
    pids_subtree_enabled: AtomicBool,
    lifecycle: AtomicU8,
    reservations: AtomicUsize,
    member_tasks: Mutex<BTreeMap<TaskIdentityKey, Arc<kidentity::PidHandle>>>,
}

#[derive(Clone, Copy, Eq, Ord, PartialEq, PartialOrd)]
struct TaskIdentityKey(usize);

impl TaskIdentityKey {
    fn new(task: &Arc<kidentity::PidHandle>) -> Self {
        Self(Arc::as_ptr(task).addr())
    }
}

struct CgroupHierarchy {
    transaction: Mutex<()>,
}

struct PidsController {
    is_active: AtomicBool,
    max: AtomicUsize,
    current: AtomicUsize,
}

impl PidsController {
    fn new(current: usize) -> Self {
        Self {
            is_active: AtomicBool::new(true),
            max: AtomicUsize::new(Cgroup::UNLIMITED),
            current: AtomicUsize::new(current),
        }
    }

    fn activate(&self) {
        self.max.store(Cgroup::UNLIMITED, Ordering::Release);
        self.is_active.store(true, Ordering::Release);
    }

    fn deactivate(&self) {
        self.is_active.store(false, Ordering::Release);
        self.max.store(Cgroup::UNLIMITED, Ordering::Release);
    }

    fn reserve_task(&self) -> KResult<()> {
        let mut current = self.current.load(Ordering::Acquire);
        loop {
            if self.is_active.load(Ordering::Acquire) && current >= self.max.load(Ordering::Acquire)
            {
                return Err(KError::from(LinuxError::EAGAIN));
            }
            match self.current.compare_exchange_weak(
                current,
                current
                    .checked_add(1)
                    .ok_or_else(|| KError::from(LinuxError::EAGAIN))?,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return Ok(()),
                Err(observed) => current = observed,
            }
        }
    }

    fn add_migrated_task(&self) -> KResult<()> {
        self.current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_add(1)
            })
            .map(|_| ())
            .map_err(|_| KError::from(LinuxError::EAGAIN))
    }

    fn release_task(&self) {
        let result = self
            .current
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                current.checked_sub(1)
            });
        // A failed decrement indicates a broken membership invariant. Keep
        // the counter unchanged in release builds so corruption cannot wrap
        // it to `usize::MAX` and permanently block admission.
        debug_assert!(result.is_ok(), "cgroup pids count underflow");
    }
}

impl Cgroup {
    const ACTIVE: u8 = 0;
    /// Largest numeric `pids.max` value accepted by Linux's PID domain.
    pub const PIDS_MAX_LIMIT: usize = 4 * 1024 * 1024;
    const REMOVED: u8 = 2;
    const REMOVING: u8 = 1;
    const UNLIMITED: usize = usize::MAX;

    fn new_root() -> Arc<Self> {
        let hierarchy = Arc::new(CgroupHierarchy {
            transaction: Mutex::new(()),
        });
        Arc::new(Self {
            name: String::new(),
            parent: Weak::new(),
            hierarchy,
            children: RwLock::new(BTreeMap::new()),
            pids: RwLock::new(None),
            pids_subtree_enabled: AtomicBool::new(false),
            lifecycle: AtomicU8::new(Self::ACTIVE),
            reservations: AtomicUsize::new(0),
            member_tasks: Mutex::new(BTreeMap::new()),
        })
    }

    /// Returns this node's local name. The hierarchy root has an empty name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Creates a direct child node.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` for an empty, dot, dot-dot, or slash-containing name,
    /// `ENOENT` for an inactive parent, or `EEXIST` for a duplicate child name.
    pub fn create_child(self: &Arc<Self>, name: &str) -> KResult<Arc<Self>> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(KError::from(LinuxError::EINVAL));
        }
        let _transaction = self.hierarchy.transaction.lock();
        let mut children = self.children.write();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        if children.contains_key(name) {
            return Err(KError::from(LinuxError::EEXIST));
        }
        let child = Arc::new(Self {
            name: name.to_string(),
            parent: Arc::downgrade(self),
            hierarchy: self.hierarchy.clone(),
            children: RwLock::new(BTreeMap::new()),
            pids: RwLock::new(
                self.pids_subtree_enabled()
                    .then(|| Arc::new(PidsController::new(0))),
            ),
            pids_subtree_enabled: AtomicBool::new(false),
            lifecycle: AtomicU8::new(Self::ACTIVE),
            reservations: AtomicUsize::new(0),
            member_tasks: Mutex::new(BTreeMap::new()),
        });
        children.insert(name.to_string(), child.clone());
        Ok(child)
    }

    /// Looks up a direct child node.
    pub fn child(&self, name: &str) -> Option<Arc<Self>> {
        self.children.read().get(name).cloned()
    }

    /// Returns a snapshot of direct child names.
    pub fn child_names(&self) -> Vec<String> {
        self.children.read().keys().cloned().collect()
    }

    /// Removes an empty direct child node.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` if the named child is absent, or `EBUSY` if it is not
    /// active or still has children, members, or lifecycle reservations.
    pub fn remove_child(&self, name: &str) -> KResult<()> {
        let _transaction = self.hierarchy.transaction.lock();
        let child = self
            .children
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| KError::from(LinuxError::ENOENT))?;
        self.remove_child_exact_locked(&child)
    }

    /// Removes the exact child node previously resolved by the caller.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` unless the parent contains this exact child object.
    /// Returns `EBUSY` if removal is already active or children, members, or
    /// reservations remain.
    pub fn remove_child_node(self: &Arc<Self>, child: &Arc<Self>) -> KResult<()> {
        let _transaction = self.hierarchy.transaction.lock();
        self.remove_child_exact_locked(child)
    }

    fn remove_child_exact_locked(&self, child: &Arc<Self>) -> KResult<()> {
        let mut children = self.children.write();
        let current = children
            .get(child.name())
            .filter(|current| Arc::ptr_eq(current, child))
            .cloned()
            .ok_or_else(|| KError::from(LinuxError::ENOENT))?;
        child
            .lifecycle
            .compare_exchange(
                Self::ACTIVE,
                Self::REMOVING,
                Ordering::AcqRel,
                Ordering::Acquire,
            )
            .map_err(|_| KError::from(LinuxError::EBUSY))?;
        if !child.member_tasks.lock().is_empty()
            || !child.children.read().is_empty()
            || child.reservations.load(Ordering::Acquire) != 0
        {
            child.lifecycle.store(Self::ACTIVE, Ordering::Release);
            return Err(KError::from(LinuxError::EBUSY));
        }
        children.remove(current.name());
        if let Some(pids) = child.pids.read().as_ref() {
            pids.deactivate();
        }
        child.lifecycle.store(Self::REMOVED, Ordering::Release);
        Ok(())
    }

    /// Acquires a live reference for one filesystem operation.
    ///
    /// Removal cannot complete while the returned guard is alive. Operations
    /// started after removal receive `ENODEV`.
    ///
    /// # Errors
    ///
    /// Returns `ENODEV` if the node is not active. Keep the guard alive through
    /// the complete filesystem operation; it guarantees liveness, not authorization.
    ///
    /// See the [crate example](crate) for controller setup and the
    /// reserve/commit/detach lifecycle. Filesystem adapters additionally acquire an
    /// operation guard before using a node and drop it when the operation finishes.
    pub fn begin_operation(self: &Arc<Self>) -> KResult<CgroupOperationGuard> {
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENODEV));
        }
        self.reservations.fetch_add(1, Ordering::AcqRel);
        Ok(CgroupOperationGuard {
            cgroup: self.clone(),
        })
    }

    /// Returns whether this node is inside `root`'s subtree, including `root`.
    pub fn is_descendant_of(self: &Arc<Self>, root: &Arc<Self>) -> bool {
        let _transaction = self.hierarchy.transaction.lock();
        self.lineage().iter().any(|node| Arc::ptr_eq(node, root))
    }

    /// Returns the nearest common ancestor in the same hierarchy.
    ///
    /// # Errors
    ///
    /// Returns `EXDEV` if the nodes belong to different hierarchies or their
    /// live parent lineages have no common ancestor.
    pub fn common_ancestor(self: &Arc<Self>, other: &Arc<Self>) -> KResult<Arc<Self>> {
        let _transaction = self.hierarchy.transaction.lock();
        if !Arc::ptr_eq(&self.hierarchy, &other.hierarchy) {
            return Err(KError::from(LinuxError::EXDEV));
        }
        let left = self.lineage();
        let right = other.lineage();
        left.into_iter()
            .zip(right)
            .take_while(|(left, right)| Arc::ptr_eq(left, right))
            .map(|(node, _)| node)
            .last()
            .ok_or_else(|| KError::from(LinuxError::EXDEV))
    }

    /// Returns the absolute path in the unified hierarchy.
    pub fn path(&self) -> String {
        if self.name.is_empty() {
            return "/".to_string();
        }
        let mut components = Vec::new();
        components.push(self.name.clone());
        let mut cursor = self.parent.upgrade();
        while let Some(node) = cursor {
            if !node.name.is_empty() {
                components.push(node.name.clone());
            }
            cursor = node.parent.upgrade();
        }
        components.reverse();
        let mut path = String::new();
        for component in components {
            path.push('/');
            path.push_str(&component);
        }
        path
    }

    /// Returns this node's path as observed from a cgroup namespace root.
    ///
    /// Nodes below `view_root` are rendered from `/`. Nodes outside that
    /// subtree retain the Linux cgroup-namespace `..` components needed to
    /// describe their position relative to the view root.
    ///
    /// # Errors
    ///
    /// Returns `EXDEV` when `view_root` belongs to another hierarchy.
    pub fn path_from(self: &Arc<Self>, view_root: &Arc<Self>) -> KResult<String> {
        let node_lineage = self.lineage();
        let root_lineage = view_root.lineage();
        if !Arc::ptr_eq(&node_lineage[0], &root_lineage[0]) {
            return Err(KError::from(LinuxError::EXDEV));
        }
        let shared = node_lineage
            .iter()
            .zip(&root_lineage)
            .take_while(|(node, root)| Arc::ptr_eq(node, root))
            .count();

        let mut path = String::new();
        for _ in shared..root_lineage.len() {
            path.push_str("/..");
        }
        for node in &node_lineage[shared..] {
            if !node.name.is_empty() {
                path.push('/');
                path.push_str(&node.name);
            }
        }
        if path.is_empty() {
            Ok("/".to_string())
        } else {
            Ok(path)
        }
    }

    /// Returns the root of this node's hierarchy.
    pub fn hierarchy_root(self: &Arc<Self>) -> Arc<Self> {
        let mut root = self.clone();
        while let Some(parent) = root.parent.upgrade() {
            root = parent;
        }
        root
    }

    /// Returns whether this node has an active pids controller.
    pub fn has_pids_controller(&self) -> bool {
        self.pids
            .read()
            .as_ref()
            .is_some_and(|pids| pids.is_active.load(Ordering::Acquire))
    }

    /// Returns whether `pids` may be enabled for this node's children.
    pub fn has_available_pids_controller(&self) -> bool {
        self.name.is_empty() || self.has_pids_controller()
    }

    /// Returns the configured task limit, or `None` for `max`.
    ///
    /// # Errors
    ///
    /// Returns `ENODEV` for an inactive node, or `ENOENT` if no active pids
    /// controller is attached (including the hierarchy root).
    pub fn pids_max(&self) -> KResult<Option<usize>> {
        let pids = self.active_pids_controller()?;
        Ok(match pids.max.load(Ordering::Acquire) {
            Self::UNLIMITED => None,
            limit => Some(limit),
        })
    }

    /// Changes the task limit. A value below `pids.current` is valid and only
    /// prevents later task creation.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` for a numeric limit above [`Self::PIDS_MAX_LIMIT`],
    /// `ENODEV` for an inactive node, or `ENOENT` without an active controller.
    ///
    /// See the [crate example](crate) for controller setup and the
    /// reserve/commit/detach lifecycle. Filesystem adapters additionally acquire an
    /// operation guard before using a node and drop it when the operation finishes.
    pub fn set_pids_max(&self, limit: Option<usize>) -> KResult<()> {
        if limit.is_some_and(|limit| limit > Self::PIDS_MAX_LIMIT) {
            return Err(KError::from(LinuxError::EINVAL));
        }
        self.active_pids_controller()?
            .max
            .store(limit.unwrap_or(Self::UNLIMITED), Ordering::Release);
        Ok(())
    }

    /// Returns the number of live and reserved tasks charged to this node.
    ///
    /// # Errors
    ///
    /// Returns `ENODEV` for an inactive node, or `ENOENT` without an active
    /// pids controller (including the hierarchy root).
    pub fn pids_current(&self) -> KResult<usize> {
        Ok(self
            .active_pids_controller()?
            .current
            .load(Ordering::Acquire))
    }

    /// Returns whether the pids controller is delegated to direct children.
    pub fn pids_subtree_enabled(&self) -> bool {
        self.pids_subtree_enabled.load(Ordering::Acquire)
    }

    /// Enables or disables pids control in direct children.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` for an inactive node, `EINVAL` when enabling a controller
    /// not available from the parent, or `EBUSY` when enabling on a non-root node
    /// with direct members or disabling while a child still delegates the controller.
    ///
    /// See the [crate example](crate) for controller setup and the
    /// reserve/commit/detach lifecycle. Filesystem adapters additionally acquire an
    /// operation guard before using a node and drop it when the operation finishes.
    pub fn set_pids_subtree_enabled(&self, enabled: bool) -> KResult<()> {
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        if enabled == self.pids_subtree_enabled() {
            return Ok(());
        }

        let children = self.children.read();
        if enabled {
            if !self.has_available_pids_controller() {
                return Err(KError::from(LinuxError::EINVAL));
            }
            if !self.name.is_empty() && !self.member_tasks.lock().is_empty() {
                return Err(KError::from(LinuxError::EBUSY));
            }
            for child in children.values() {
                let subtree_tasks = child.subtree_task_count();
                let mut pids = child.pids.write();
                if let Some(pids) = pids.as_ref() {
                    pids.activate();
                } else {
                    *pids = Some(Arc::new(PidsController::new(subtree_tasks)));
                }
            }
        } else {
            if children.values().any(|child| child.pids_subtree_enabled()) {
                return Err(KError::from(LinuxError::EBUSY));
            }
            for child in children.values() {
                if let Some(pids) = child.pids.read().as_ref() {
                    pids.deactivate();
                }
            }
        }
        self.pids_subtree_enabled.store(enabled, Ordering::Release);
        Ok(())
    }

    /// Returns stable task identities directly attached to this node.
    pub fn member_tasks(&self) -> Vec<Arc<kidentity::PidHandle>> {
        self.member_tasks.lock().values().cloned().collect()
    }

    /// Reserves one task charge for fork or clone.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` for an inactive node, or `EAGAIN` when an active
    /// controller limit is reached or a count would overflow. Partial charges
    /// are rolled back before returning an error.
    ///
    /// See the [crate example](crate) for controller setup and the
    /// reserve/commit/detach lifecycle. Filesystem adapters additionally acquire an
    /// operation guard before using a node and drop it when the operation finishes.
    pub fn reserve_task(self: &Arc<Self>) -> KResult<TaskCharge> {
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        let mut charge = TaskCharge {
            cgroup: self.clone(),
            controllers: self.controller_lineage(),
            charged: 0,
            reservation_held: true,
        };
        self.reservations.fetch_add(1, Ordering::AcqRel);
        for controller in &charge.controllers {
            controller.reserve_task()?;
            charge.charged += 1;
        }
        Ok(charge)
    }

    fn lineage(self: &Arc<Self>) -> Vec<Arc<Self>> {
        let mut nodes = Vec::new();
        nodes.push(self.clone());
        let mut cursor = self.parent.upgrade();
        while let Some(node) = cursor {
            cursor = node.parent.upgrade();
            nodes.push(node);
        }
        nodes.reverse();
        nodes
    }

    fn active_pids_controller(&self) -> KResult<Arc<PidsController>> {
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENODEV));
        }
        self.pids
            .read()
            .as_ref()
            .filter(|pids| pids.is_active.load(Ordering::Acquire))
            .cloned()
            .ok_or_else(|| KError::from(LinuxError::ENOENT))
    }

    fn controller_lineage(self: &Arc<Self>) -> Vec<Arc<PidsController>> {
        self.lineage()
            .into_iter()
            .filter_map(|cgroup| cgroup.pids.read().clone())
            .collect()
    }

    fn subtree_task_count(&self) -> usize {
        self.member_tasks.lock().len()
            + self
                .children
                .read()
                .values()
                .map(|child| child.subtree_task_count())
                .sum::<usize>()
    }

    fn is_active(&self) -> bool {
        self.lifecycle.load(Ordering::Acquire) == Self::ACTIVE
    }
}

/// Live cgroup reference held for the duration of one filesystem operation.
pub struct CgroupOperationGuard {
    cgroup: Arc<Cgroup>,
}

impl Drop for CgroupOperationGuard {
    fn drop(&mut self) {
        self.cgroup.reservations.fetch_sub(1, Ordering::AcqRel);
    }
}

/// A fork/clone task charge that rolls back unless converted into membership.
pub struct TaskCharge {
    cgroup: Arc<Cgroup>,
    controllers: Vec<Arc<PidsController>>,
    charged: usize,
    reservation_held: bool,
}

impl TaskCharge {
    /// Commits the reservation for a stable task identity.
    ///
    /// # Errors
    ///
    /// Returns `EEXIST` when that identity already has membership in this
    /// cgroup. The reserved controller charge is rolled back on failure.
    ///
    /// # Panics
    ///
    /// Panics if not every controller was charged or the reserved node is inactive.
    /// Successful reservation and its retained lifecycle reservation maintain these
    /// invariants until commit or rollback.
    ///
    /// See the [crate example](crate) for controller setup and the
    /// reserve/commit/detach lifecycle. Filesystem adapters additionally acquire an
    /// operation guard before using a node and drop it when the operation finishes.
    pub fn commit(mut self, task: Arc<kidentity::PidHandle>) -> KResult<TaskMembership> {
        let task_key = TaskIdentityKey::new(&task);
        assert_eq!(
            self.charged,
            self.controllers.len(),
            "incomplete task charge"
        );
        let cgroup = self.cgroup.clone();
        {
            let _transaction = cgroup.hierarchy.transaction.lock();
            assert!(
                cgroup.is_active(),
                "committing a task charge to inactive cgroup"
            );
            let mut member_tasks = cgroup.member_tasks.lock();
            if member_tasks.contains_key(&task_key) {
                return Err(KError::from(LinuxError::EEXIST));
            }
            member_tasks.insert(task_key, task.clone());
            cgroup.reservations.fetch_sub(1, Ordering::AcqRel);
        }
        self.reservation_held = false;
        self.charged = 0;
        Ok(TaskMembership {
            task_key,
            task,
            cgroup: RwLock::new(Some(cgroup)),
        })
    }
}

impl Drop for TaskCharge {
    fn drop(&mut self) {
        if self.reservation_held {
            self.cgroup.reservations.fetch_sub(1, Ordering::AcqRel);
        }
        for controller in self.controllers[..self.charged].iter().rev() {
            controller.release_task();
        }
    }
}

/// Canonical cgroup membership owned by one task/thread.
pub struct TaskMembership {
    task_key: TaskIdentityKey,
    task: Arc<kidentity::PidHandle>,
    cgroup: RwLock<Option<Arc<Cgroup>>>,
}

impl TaskMembership {
    /// Returns the current cgroup node.
    pub fn cgroup(&self) -> Option<Arc<Cgroup>> {
        self.cgroup.read().clone()
    }

    /// Moves an existing task without applying `pids.max` to the target.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::migrate_group`] errors: `ENOENT` for an inactive target,
    /// `EBUSY` for a non-root delegating target, `EXDEV` for another hierarchy,
    /// and `EAGAIN` for controller-count overflow.
    pub fn migrate(&self, target: &Arc<Cgroup>) -> KResult<()> {
        Self::migrate_group(&[self], target)
    }

    /// Moves a fixed set of tasks as one membership transaction.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` for an inactive target, `EBUSY` if a non-root target
    /// delegates controllers, `EXDEV` for any attached source in another hierarchy,
    /// or `EAGAIN` on controller-count overflow. Validation precedes mutation and
    /// failed additions are rolled back. Detached members are skipped.
    pub fn migrate_group(memberships: &[&Self], target: &Arc<Cgroup>) -> KResult<()> {
        let _transaction = target.hierarchy.transaction.lock();
        if !target.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        if !target.name.is_empty() && target.pids_subtree_enabled() {
            return Err(KError::from(LinuxError::EBUSY));
        }
        let mut ordered = memberships.to_vec();
        ordered.sort_unstable_by_key(|membership| membership.task_key);
        ordered.dedup_by_key(|membership| membership.task_key);

        let mut current = Vec::with_capacity(ordered.len());
        for membership in &ordered {
            current.push(membership.cgroup.write());
        }
        let target_lineage = target.controller_lineage();
        // Validate every source before mutating any controller count. This
        // keeps a mixed-hierarchy request atomic on its error path.
        let mut additions: Vec<Arc<PidsController>> = Vec::new();
        for source in &current {
            if let Some(source_cgroup) = source.as_ref()
                && !Arc::ptr_eq(&source_cgroup.hierarchy, &target.hierarchy)
            {
                return Err(KError::from(LinuxError::EXDEV));
            }
        }
        for source in &current {
            let Some(source_cgroup) = source.as_ref() else {
                continue;
            };
            if Arc::ptr_eq(source_cgroup, target) {
                continue;
            }
            let source_lineage = source_cgroup.controller_lineage();
            let shared = source_lineage
                .iter()
                .zip(&target_lineage)
                .take_while(|(source, target)| Arc::ptr_eq(source, target))
                .count();
            for controller in &target_lineage[shared..] {
                if let Err(error) = controller.add_migrated_task() {
                    for added in additions.iter().rev() {
                        added.release_task();
                    }
                    return Err(error);
                }
                additions.push(controller.clone());
            }
        }

        for (membership, source) in ordered.into_iter().zip(current.iter_mut()) {
            let Some(source_cgroup) = source.as_ref() else {
                continue;
            };
            if Arc::ptr_eq(source_cgroup, target) {
                continue;
            }
            let source_lineage = source_cgroup.controller_lineage();
            let shared = source_lineage
                .iter()
                .zip(&target_lineage)
                .take_while(|(source, target)| Arc::ptr_eq(source, target))
                .count();
            target
                .member_tasks
                .lock()
                .insert(membership.task_key, membership.task.clone());
            source_cgroup
                .member_tasks
                .lock()
                .remove(&membership.task_key);
            for controller in source_lineage[shared..].iter().rev() {
                controller.release_task();
            }
            **source = Some(target.clone());
        }
        Ok(())
    }

    /// Reserves a child task in the same cgroup.
    ///
    /// # Errors
    ///
    /// Returns `ESRCH` if membership is detached; otherwise propagates
    /// [`Cgroup::reserve_task`] errors (`ENOENT` or `EAGAIN`).
    pub fn reserve_child(&self) -> KResult<TaskCharge> {
        let cgroup = self
            .cgroup
            .read()
            .clone()
            .ok_or_else(|| KError::from(LinuxError::ESRCH))?;
        cgroup.reserve_task()
    }

    /// Detaches an exiting task and releases its hierarchical charge.
    pub fn detach(&self) {
        self.do_detach();
    }

    fn do_detach(&self) {
        let cgroup_snapshot = self.cgroup.read().clone();
        let Some(cgroup_snapshot) = cgroup_snapshot else {
            return;
        };
        let _transaction = cgroup_snapshot.hierarchy.transaction.lock();
        let Some(cgroup) = self.cgroup.write().take() else {
            return;
        };
        cgroup.member_tasks.lock().remove(&self.task_key);
        for controller in cgroup.controller_lineage().iter().rev() {
            controller.release_task();
        }
    }
}

impl Drop for TaskMembership {
    fn drop(&mut self) {
        self.do_detach();
    }
}

/// Cgroup namespace view.
pub struct CgroupNamespace {
    id: CgroupNamespaceId,
    root: Arc<Cgroup>,
}

impl Default for CgroupNamespace {
    fn default() -> Self {
        Self::new()
    }
}

impl CgroupNamespace {
    /// Creates an independent namespace view and hierarchy root.
    pub fn new() -> Self {
        Self {
            id: CgroupNamespaceId::new(),
            root: Cgroup::new_root(),
        }
    }

    /// Returns the system's initial cgroup namespace.
    ///
    /// The initial namespace and its hierarchy are available during boot,
    /// before a current user process exists.
    pub fn initial() -> Arc<Self> {
        Arc::clone(INITIAL_CGROUP_NAMESPACE.call_once(|| Arc::new(Self::new())))
    }

    /// Creates another namespace view rooted at an existing cgroup.
    pub fn new_view(root: Arc<Cgroup>) -> Self {
        Self {
            id: CgroupNamespaceId::new(),
            root,
        }
    }

    /// Returns the cgroup namespace identifier.
    pub fn id(&self) -> CgroupNamespaceId {
        self.id
    }

    /// Returns the namespace-visible hierarchy root.
    pub fn root(&self) -> Arc<Cgroup> {
        self.root.clone()
    }
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
