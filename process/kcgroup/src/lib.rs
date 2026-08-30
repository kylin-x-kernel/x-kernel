// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup v2 hierarchy and task membership.
//!
//! This crate owns the kernel's canonical cgroup state. Filesystems and Linux
//! syscall compatibility code are adapters over these objects; they must not
//! maintain parallel membership or controller state.

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
    pub fn pids_max(&self) -> KResult<Option<usize>> {
        let pids = self.active_pids_controller()?;
        Ok(match pids.max.load(Ordering::Acquire) {
            Self::UNLIMITED => None,
            limit => Some(limit),
        })
    }

    /// Changes the task limit. A value below `pids.current` is valid and only
    /// prevents later task creation.
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
    pub fn migrate(&self, target: &Arc<Cgroup>) -> KResult<()> {
        Self::migrate_group(&[self], target)
    }

    /// Moves a fixed set of tasks as one membership transaction.
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

#[cfg(unittest)]
mod tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn initial_namespace_is_shared() {
        assert!(Arc::ptr_eq(
            &CgroupNamespace::initial(),
            &CgroupNamespace::initial()
        ));
    }

    fn make_pids_group(name: &str) -> (Arc<Cgroup>, Arc<Cgroup>) {
        let root = Cgroup::new_root();
        root.set_pids_subtree_enabled(true).unwrap();
        let child = root.create_child(name).unwrap();
        (root, child)
    }

    #[def_test]
    fn failed_reservation_does_not_leak_a_charge() {
        let (_root, group) = make_pids_group("limited");
        group.set_pids_max(Some(1)).unwrap();
        let membership = group
            .reserve_task()
            .unwrap()
            .commit(kidentity::PidHandle::fixed_root(1))
            .unwrap();
        assert_eq!(
            group.reserve_task().err(),
            Some(KError::from(LinuxError::EAGAIN))
        );
        assert_eq!(group.pids_current(), Ok(1));
        drop(membership);
        assert_eq!(group.pids_current(), Ok(0));
    }

    #[def_test]
    fn migration_ignores_target_limit_and_transfers_charge() {
        let root = Cgroup::new_root();
        root.set_pids_subtree_enabled(true).unwrap();
        let source = root.create_child("source").unwrap();
        let target = root.create_child("target").unwrap();
        target.set_pids_max(Some(0)).unwrap();
        let membership = source
            .reserve_task()
            .unwrap()
            .commit(kidentity::PidHandle::fixed_root(7))
            .unwrap();
        membership.migrate(&target).unwrap();
        assert_eq!(source.pids_current(), Ok(0));
        assert_eq!(target.pids_current(), Ok(1));
        assert_eq!(membership.cgroup().unwrap().path(), "/target");
    }

    #[def_test]
    fn lowering_limit_below_current_is_allowed() {
        let (_root, group) = make_pids_group("limited");
        let _first = group
            .reserve_task()
            .unwrap()
            .commit(kidentity::PidHandle::fixed_root(1))
            .unwrap();
        let _second = group
            .reserve_task()
            .unwrap()
            .commit(kidentity::PidHandle::fixed_root(2))
            .unwrap();
        group.set_pids_max(Some(1)).unwrap();
        assert_eq!(group.pids_current(), Ok(2));
        assert!(group.reserve_task().is_err());
    }

    #[def_test]
    fn pids_limit_rejects_values_outside_the_linux_pid_domain() {
        let (_root, group) = make_pids_group("bounded");

        assert_eq!(group.set_pids_max(Some(Cgroup::PIDS_MAX_LIMIT)), Ok(()));
        assert_eq!(group.pids_max(), Ok(Some(Cgroup::PIDS_MAX_LIMIT)));
        assert_eq!(
            group.set_pids_max(Some(Cgroup::PIDS_MAX_LIMIT + 1)),
            Err(KError::from(LinuxError::EINVAL))
        );
        assert_eq!(group.pids_max(), Ok(Some(Cgroup::PIDS_MAX_LIMIT)));
    }

    #[def_test]
    fn operation_guard_blocks_removal_and_old_handles_fail_after_removal() {
        let (root, child) = make_pids_group("guarded");
        let operation = child.begin_operation().unwrap();

        assert_eq!(
            root.remove_child("guarded"),
            Err(KError::from(LinuxError::EBUSY))
        );
        drop(operation);
        root.remove_child("guarded").unwrap();

        assert_eq!(
            child.begin_operation().err(),
            Some(KError::from(LinuxError::ENODEV))
        );
        assert_eq!(
            child.set_pids_max(Some(1)),
            Err(KError::from(LinuxError::ENODEV))
        );
    }

    #[def_test]
    fn subtree_visibility_and_common_ancestor_use_stable_node_identity() {
        let root = Cgroup::new_root();
        let delegated = root.create_child("delegated").unwrap();
        let left = delegated.create_child("left").unwrap();
        let right = delegated.create_child("right").unwrap();
        let outside = root.create_child("outside").unwrap();

        assert!(left.is_descendant_of(&delegated));
        assert!(delegated.is_descendant_of(&delegated));
        assert!(!outside.is_descendant_of(&delegated));
        assert!(Arc::ptr_eq(
            &left.common_ancestor(&right).unwrap(),
            &delegated
        ));
        assert!(Arc::ptr_eq(&left.common_ancestor(&outside).unwrap(), &root));
        assert_eq!(
            left.common_ancestor(&Cgroup::new_root()).err(),
            Some(KError::from(LinuxError::EXDEV))
        );
    }

    #[def_test]
    fn namespace_paths_are_relative_to_the_view_root() {
        let root = Cgroup::new_root();
        let left = root.create_child("left").unwrap();
        let nested = left.create_child("nested").unwrap();
        let right = root.create_child("right").unwrap();

        assert_eq!(left.path_from(&left), Ok("/".to_string()));
        assert_eq!(nested.path_from(&left), Ok("/nested".to_string()));
        assert_eq!(right.path_from(&left), Ok("/../right".to_string()));
    }

    #[def_test]
    fn namespace_path_rejects_another_hierarchy() {
        let left = CgroupNamespace::new().root();
        let right = CgroupNamespace::new().root();

        assert_eq!(left.path_from(&right), Err(KError::from(LinuxError::EXDEV)));
    }

    #[def_test]
    fn duplicate_identity_commit_preserves_membership_and_rolls_back_charge() {
        let (root, group) = make_pids_group("duplicate");
        let identity = kidentity::PidHandle::fixed_root(42);
        let membership = group
            .reserve_task()
            .unwrap()
            .commit(identity.clone())
            .unwrap();

        assert_eq!(
            group.reserve_task().unwrap().commit(identity.clone()).err(),
            Some(KError::from(LinuxError::EEXIST))
        );
        assert_eq!(group.pids_current(), Ok(1));
        let members = group.member_tasks();
        assert_eq!(members.len(), 1);
        assert!(Arc::ptr_eq(&members[0], &identity));
        assert_eq!(
            root.remove_child("duplicate"),
            Err(KError::from(LinuxError::EBUSY))
        );

        drop(membership);
        assert_eq!(group.pids_current(), Ok(0));
        root.remove_child("duplicate").unwrap();
    }

    #[def_test]
    fn reused_numeric_projection_keeps_distinct_stable_identities() {
        let (_root, group) = make_pids_group("reused");
        let first_identity = kidentity::PidHandle::fixed_root(43);
        let second_identity = kidentity::PidHandle::fixed_root(43);
        let first = group
            .reserve_task()
            .unwrap()
            .commit(first_identity.clone())
            .unwrap();
        let second = group
            .reserve_task()
            .unwrap()
            .commit(second_identity.clone())
            .unwrap();

        let members = group.member_tasks();
        assert_eq!(members.len(), 2);
        assert!(
            members
                .iter()
                .any(|member| Arc::ptr_eq(member, &first_identity))
        );
        assert!(
            members
                .iter()
                .any(|member| Arc::ptr_eq(member, &second_identity))
        );

        drop(first);
        drop(second);
        assert_eq!(group.pids_current(), Ok(0));
    }

    #[def_test]
    fn removed_cgroup_rejects_reservation_and_migration() {
        let root = Cgroup::new_root();
        let removed = root.create_child("removed").unwrap();
        root.remove_child("removed").unwrap();

        assert_eq!(
            removed.reserve_task().err(),
            Some(KError::from(LinuxError::ENOENT))
        );
        let membership = root
            .reserve_task()
            .unwrap()
            .commit(kidentity::PidHandle::fixed_root(9))
            .unwrap();
        assert_eq!(
            membership.migrate(&removed).err(),
            Some(KError::from(LinuxError::ENOENT))
        );
        assert!(Arc::ptr_eq(&membership.cgroup().unwrap(), &root));
        assert_eq!(root.member_tasks()[0].root_nr(), 9);
        assert!(!removed.has_pids_controller());
    }

    #[def_test]
    fn failed_busy_removal_keeps_cgroup_active() {
        let root = Cgroup::new_root();
        let child = root.create_child("busy").unwrap();
        let membership = child
            .reserve_task()
            .unwrap()
            .commit(kidentity::PidHandle::fixed_root(10))
            .unwrap();

        assert_eq!(
            root.remove_child("busy").err(),
            Some(KError::from(LinuxError::EBUSY))
        );
        assert!(child.reserve_task().is_ok());
        drop(membership);
    }

    #[def_test]
    fn detached_membership_is_observable_without_panicking() {
        let (_root, group) = make_pids_group("member");
        let membership = group
            .reserve_task()
            .unwrap()
            .commit(kidentity::PidHandle::fixed_root(11))
            .unwrap();

        membership.detach();
        membership.detach();
        assert!(membership.cgroup().is_none());
        assert_eq!(group.pids_current(), Ok(0));
    }

    #[def_test]
    fn pids_controller_is_not_exposed_on_the_hierarchy_root() {
        let root = Cgroup::new_root();

        assert!(!root.has_pids_controller());
        assert!(root.has_available_pids_controller());
        assert_eq!(root.pids_current(), Err(KError::from(LinuxError::ENOENT)));
    }

    #[def_test]
    fn subtree_control_activates_controller_for_direct_children() {
        let root = Cgroup::new_root();
        let child = root.create_child("child").unwrap();
        let membership = child
            .reserve_task()
            .unwrap()
            .commit(kidentity::PidHandle::fixed_root(12))
            .unwrap();

        root.set_pids_subtree_enabled(true).unwrap();
        assert!(child.has_pids_controller());
        assert_eq!(child.pids_current(), Ok(1));
        drop(membership);
        assert_eq!(child.pids_current(), Ok(0));
    }

    #[def_test]
    fn non_root_internal_process_blocks_controller_delegation() {
        let (_root, child) = make_pids_group("child");
        let _membership = child
            .reserve_task()
            .unwrap()
            .commit(kidentity::PidHandle::fixed_root(13))
            .unwrap();

        assert_eq!(
            child.set_pids_subtree_enabled(true),
            Err(KError::from(LinuxError::EBUSY))
        );
    }

    #[def_test]
    fn outstanding_reservation_blocks_removal_until_commit_or_drop() {
        let (root, child) = make_pids_group("reserved");
        let charge = child.reserve_task().unwrap();
        assert_eq!(
            root.remove_child("reserved"),
            Err(KError::from(LinuxError::EBUSY))
        );
        drop(charge);
        root.remove_child("reserved").unwrap();
    }
}
