// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup v2 hierarchy nodes, controller activation, and the namespace view.
//!
//! # Lock order
//!
//! Mutating operations take the hierarchy-wide `transaction` mutex first, then
//! node `children`/`member_tasks` locks, then controller atomics. Scheduling
//! code that needs both a task lock and a cgroup controller must take the task
//! lock first; cgroup code never takes a task lock while holding a controller.
//!
//! # Lifecycle
//!
//! A node is `ACTIVE` while it accepts new charges, `REMOVING` while a removal
//! is validating, and `REMOVED` once unlinked. The `REMOVING` state is a
//! tombstone that keeps new reservations out while an in-flight operation
//! finishes.

use alloc::{
    collections::{BTreeMap, BTreeSet},
    string::{String, ToString},
    sync::{Arc, Weak},
    vec::Vec,
};
use core::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use kerrno::{KError, KResult, LinuxError};
use kidentity::TaskIdentityToken;
use kspin::SpinNoIrq;
use ksync::{Mutex, RwLock};
use memacct::Memcg;

use crate::{
    controller::{
        Controller, ControllerAnchor, ControllerKind, ControllerSet, CpuController, CpuMask,
        CpusetController, MemoryController, NodeMask, PidsController, registrations,
    },
    membership::{CgroupOperationGuard, TaskCharge},
};

const ACTIVE: u8 = 0;
const REMOVING: u8 = 1;
const REMOVED: u8 = 2;

/// Hierarchy-wide state shared by every node of one cgroup namespace root.
pub(crate) struct CgroupHierarchy {
    /// Serializes structural changes, deletions, and multi-controller charges.
    pub(crate) transaction: Mutex<()>,
    /// CPUs present on the machine; the root's `cpuset.cpus.effective`.
    cpuset_universe: SpinNoIrq<CpuMask>,
    /// NUMA nodes present on the machine; the root's `cpuset.mems.effective`.
    mems_universe: NodeMask,
}

impl CgroupHierarchy {
    /// Returns the machine-wide CPU universe.
    pub(crate) fn cpuset_universe(&self) -> CpuMask {
        *self.cpuset_universe.lock()
    }

    pub(crate) fn set_cpuset_universe(&self, cpus: CpuMask) {
        *self.cpuset_universe.lock() = cpus;
    }

    /// Returns the machine-wide NUMA-node universe.
    pub(crate) fn mems_universe(&self) -> NodeMask {
        self.mems_universe
    }
}

/// One node of a cgroup v2 hierarchy.
///
/// Tasks are charged through [`reserve_task`](Self::reserve_task) and become
/// visible members only after [`TaskCharge::commit`] succeeds. Controller state
/// exists only while the parent's `cgroup.subtree_control` enabled it.
pub struct Cgroup {
    pub(crate) name: String,
    pub(crate) hierarchy: Arc<CgroupHierarchy>,
    pub(crate) parent: Weak<Cgroup>,
    pub(crate) children: RwLock<BTreeMap<String, Arc<Cgroup>>>,
    pub(crate) controllers: ControllerSet,
    pub(crate) lifecycle: AtomicU8,
    pub(crate) reservations: AtomicUsize,
    pub(crate) member_tasks: Mutex<BTreeSet<TaskIdentityToken>>,
    /// Accounting node retained across memory-controller disable/enable.
    pub(crate) memory_node: SpinNoIrq<Option<Arc<memacct::Memcg>>>,
}

impl Cgroup {
    /// Largest accepted `pids.max` value, matching Linux's PID domain.
    pub const PIDS_MAX_LIMIT: usize = crate::PIDS_MAX_LIMIT;

    /// Creates a hierarchy root with the architecture's online CPUs and nodes.
    ///
    /// The root's cpuset controller is active from creation: its effective mask
    /// is the universe every descendant intersects with. Its own request mask
    /// starts empty, meaning "inherit", so `cpuset.cpus` reads empty until an
    /// administrator restricts the root. Offline CPUs are excluded from the
    /// universe, so a `cpuset.cpus` write naming one fails with `ENODEV`.
    pub(crate) fn new_root() -> Arc<Self> {
        let (cpus, mems) = default_universe();
        Self::new_root_with_universe(cpus, mems)
    }

    /// Creates a hierarchy root with an explicit CPU and NUMA-node universe.
    ///
    /// Unit tests use this to exercise mask arithmetic independently of the
    /// machine's topology and of architecture initialization order.
    pub(crate) fn new_root_with_universe(cpus: CpuMask, mems: NodeMask) -> Arc<Self> {
        let hierarchy = Arc::new(CgroupHierarchy {
            transaction: Mutex::new(()),
            cpuset_universe: SpinNoIrq::new(cpus),
            mems_universe: mems,
        });
        let cpuset = Arc::new(CpusetController::new(cpus, mems));
        let controllers = ControllerSet::new_root(cpuset, memacct::root());
        // Linux exposes every available controller's interface on the root
        // cgroup itself, so `cpu.max`/`cpu.weight` exist at `/sys/fs/cgroup`.
        controllers.activate(
            ControllerKind::Cpu,
            crate::controller::ControllerInstance::Cpu(Arc::new(CpuController::new())),
        );
        let memory_node = controllers.memory().map(|controller| controller.memcg());
        Arc::new(Self {
            name: String::new(),
            hierarchy,
            parent: Weak::new(),
            children: RwLock::new(BTreeMap::new()),
            controllers,
            lifecycle: AtomicU8::new(ACTIVE),
            reservations: AtomicUsize::new(0),
            member_tasks: Mutex::new(BTreeSet::new()),
            memory_node: SpinNoIrq::new(memory_node),
        })
    }

    /// Creates a child node sharing this node's hierarchy and universes.
    fn new_child(parent: &Arc<Self>, name: &str) -> Arc<Self> {
        let child = Arc::new(Self {
            name: name.to_string(),
            hierarchy: parent.hierarchy.clone(),
            parent: Arc::downgrade(parent),
            children: RwLock::new(BTreeMap::new()),
            controllers: ControllerSet::new(),
            lifecycle: AtomicU8::new(ACTIVE),
            reservations: AtomicUsize::new(0),
            member_tasks: Mutex::new(BTreeSet::new()),
            memory_node: SpinNoIrq::new(None),
        });
        parent.activate_delegated_controllers(&child);
        child
    }

    /// Activates, on `child`, every controller this cgroup already delegates.
    ///
    /// Callers hold the hierarchy transaction. The child may already own live
    /// state (children created before delegation are activated by
    /// [`enable_subtree_control`](Self::enable_subtree_control)), so existing
    /// controllers are left untouched rather than reset.
    fn activate_delegated_controllers(&self, child: &Arc<Self>) {
        let anchor = ControllerAnchor {
            subtree_tasks: child.subtree_task_count(),
            cpus: self.effective_cpus(),
            mems: self.effective_mems(),
            memcg: self.effective_memcg(),
            memory_node: child.memory_node.lock().clone(),
        };
        for registration in registrations() {
            let kind = registration.kind();
            if self.controllers.is_subtree_enabled(kind) && !child.controllers.is_active(kind) {
                child
                    .controllers
                    .activate(kind, registration.activate(anchor.clone()));
                if kind == ControllerKind::Memory
                    && let Some(controller) = child.controllers.memory()
                {
                    *child.memory_node.lock() = Some(controller.memcg());
                }
            }
        }
    }

    /// Creates a child cgroup named `name`.
    ///
    /// # Errors
    ///
    /// Returns `EINVAL` for an empty name or one containing `/`, `.`, or `..`;
    /// `ENOENT` when this cgroup is no longer active; `EEXIST` when a sibling
    /// already uses the name.
    pub fn create_child(self: &Arc<Self>, name: &str) -> KResult<Arc<Self>> {
        if name.is_empty() || name == "." || name == ".." || name.contains('/') {
            return Err(KError::from(LinuxError::EINVAL));
        }
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        let mut children = self.children.write();
        if children.contains_key(name) {
            return Err(KError::from(LinuxError::EEXIST));
        }
        let child = Self::new_child(self, name);
        children.insert(name.to_string(), child.clone());
        Ok(child)
    }

    /// Returns the child named `name`, if present.
    pub fn child(&self, name: &str) -> Option<Arc<Self>> {
        self.children.read().get(name).cloned()
    }

    /// Returns the names of direct children in lexicographic order.
    pub fn child_names(&self) -> Vec<String> {
        self.children.read().keys().cloned().collect()
    }

    /// Returns every cgroup in this subtree, including `self`.
    pub fn descendants(&self) -> Vec<Arc<Self>> {
        let mut result = Vec::new();
        self.collect_descendants(&mut result);
        result
    }

    fn collect_descendants(&self, result: &mut Vec<Arc<Self>>) {
        let children: Vec<Arc<Self>> = self.children.read().values().cloned().collect();
        for child in children {
            result.push(child.clone());
            child.collect_descendants(result);
        }
    }

    /// Removes the child named `name`.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when no such child exists, `EBUSY` when the child still
    /// has tasks, child cgroups, or in-flight charges, and `ENODEV` when this
    /// cgroup is already removed.
    pub fn remove_child(self: &Arc<Self>, name: &str) -> KResult<()> {
        let _transaction = self.hierarchy.transaction.lock();
        self.remove_child_locked(name)
    }

    fn remove_child_locked(self: &Arc<Self>, name: &str) -> KResult<()> {
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENODEV));
        }
        let child = self
            .children
            .read()
            .get(name)
            .cloned()
            .ok_or_else(|| KError::from(LinuxError::ENOENT))?;
        self.remove_child_exact_locked(&child)
    }

    /// Removes a child node that the caller already resolved.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` unless the parent contains this exact child object.
    /// Returns `EBUSY` when `child` still has tasks, child cgroups, or
    /// in-flight charges, or when it is already being removed.
    pub fn remove_child_node(self: &Arc<Self>, child: &Arc<Self>) -> KResult<()> {
        let _transaction = self.hierarchy.transaction.lock();
        self.remove_child_exact_locked(child)
    }

    fn remove_child_exact_locked(self: &Arc<Self>, child: &Arc<Self>) -> KResult<()> {
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENODEV));
        }
        // Validate identity before mutating lifecycle: a stale object with a
        // recycled name must not remove the current child.
        if !self
            .children
            .read()
            .get(&child.name)
            .is_some_and(|current| Arc::ptr_eq(current, child))
        {
            return Err(KError::from(LinuxError::ENOENT));
        }
        if child
            .lifecycle
            .compare_exchange(ACTIVE, REMOVING, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(KError::from(LinuxError::EBUSY));
        }
        let is_empty = child.children.read().is_empty()
            && child.member_tasks.lock().is_empty()
            && child.reservations.load(Ordering::Acquire) == 0;
        if !is_empty {
            child.lifecycle.store(ACTIVE, Ordering::Release);
            return Err(KError::from(LinuxError::EBUSY));
        }
        self.children.write().remove(&child.name);
        child.deactivate_controllers();
        child.lifecycle.store(REMOVED, Ordering::Release);
        Ok(())
    }

    fn deactivate_controllers(&self) {
        let active = self.controllers.memory();
        let retained = self.memory_node.lock().take();
        if let Some(controller) = &active {
            memacct::remove(controller.memcg().id());
        } else if let Some(node) = retained {
            memacct::remove(node.id());
        }
        for controller in self.controllers.controllers() {
            controller.deactivate();
        }
    }

    /// Starts a structural operation, keeping the node from being removed.
    ///
    /// # Errors
    ///
    /// Returns `ENODEV` when the cgroup is no longer active.
    pub fn begin_operation(self: &Arc<Self>) -> KResult<CgroupOperationGuard> {
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENODEV));
        }
        self.reservations.fetch_add(1, Ordering::AcqRel);
        Ok(CgroupOperationGuard::new(self.clone()))
    }

    /// Reports whether `self` is `ancestor` or one of its descendants.
    pub fn is_descendant_of(&self, ancestor: &Arc<Self>) -> bool {
        if core::ptr::eq(self, ancestor.as_ref()) {
            return true;
        }
        let mut current = self.parent.upgrade();
        while let Some(node) = current {
            if Arc::ptr_eq(&node, ancestor) {
                return true;
            }
            current = node.parent.upgrade();
        }
        false
    }

    /// Returns the nearest common ancestor of `self` and `other`.
    ///
    /// # Errors
    ///
    /// Returns `EXDEV` when the two cgroups belong to different hierarchies.
    pub fn common_ancestor(self: &Arc<Self>, other: &Arc<Self>) -> KResult<Arc<Self>> {
        if !Arc::ptr_eq(&self.hierarchy_root(), &other.hierarchy_root()) {
            return Err(KError::from(LinuxError::EXDEV));
        }
        let self_lineage = self.lineage();
        let other_lineage = other.lineage();
        self_lineage
            .into_iter()
            .zip(other_lineage)
            .take_while(|(left, right)| Arc::ptr_eq(left, right))
            .last()
            .map(|(node, _)| node)
            .ok_or_else(|| KError::from(LinuxError::EXDEV))
    }

    /// Returns the parent cgroup, if this is not a root.
    pub fn parent(&self) -> Option<Arc<Self>> {
        self.parent.upgrade()
    }

    /// Returns the absolute path from the hierarchy root.
    pub fn path(self: &Arc<Self>) -> String {
        let lineage = self.lineage();
        let mut path = String::new();
        for node in &lineage {
            if node.name.is_empty() {
                continue;
            }
            path.push('/');
            path.push_str(&node.name);
        }
        if path.is_empty() {
            path.push('/');
        }
        path
    }

    /// Returns the path relative to a namespace view root.
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

    /// Returns the root of this hierarchy.
    pub fn hierarchy_root(self: &Arc<Self>) -> Arc<Self> {
        // `lineage` is ancestor-first, so its first entry is the root.
        self.lineage()
            .into_iter()
            .next()
            .unwrap_or_else(|| self.clone())
    }

    /// Returns this node and every ancestor in ancestor-first order.
    pub(crate) fn lineage(self: &Arc<Self>) -> Vec<Arc<Self>> {
        // Ancestor-first order: controller accounting compares lineages with a
        // shared prefix, and every controller above the deepest common cgroup
        // already accounts for the task.
        let mut lineage = Vec::new();
        let mut current = Some(self.clone());
        while let Some(node) = current {
            current = node.parent.upgrade();
            lineage.push(node);
        }
        lineage.reverse();
        lineage
    }

    /// Reports whether this cgroup still accepts new charges.
    pub fn is_active(&self) -> bool {
        self.lifecycle.load(Ordering::Acquire) == ACTIVE
    }

    /// Returns the live controllers on this cgroup, root-first.
    pub(crate) fn controller_lineage(self: &Arc<Self>) -> Vec<Arc<dyn Controller>> {
        let mut controllers = Vec::new();
        for node in self.lineage() {
            controllers.extend(node.controllers.controllers());
        }
        controllers
    }

    /// Returns the number of tasks charged to this cgroup and its descendants.
    pub fn subtree_task_count(&self) -> usize {
        let mut count = self.member_tasks.lock().len();
        let children: Vec<Arc<Self>> = self.children.read().values().cloned().collect();
        for child in children {
            count += child.subtree_task_count();
        }
        count
    }

    /// Returns the tokens of tasks directly charged to this cgroup.
    pub fn member_tasks(&self) -> Vec<TaskIdentityToken> {
        self.member_tasks.lock().iter().copied().collect()
    }

    /// Reports whether this cgroup has live state for `kind`.
    pub fn has_controller(&self, kind: ControllerKind) -> bool {
        self.controllers.is_active(kind)
    }

    /// Reports whether this cgroup has live pids state.
    pub fn has_pids_controller(&self) -> bool {
        self.has_controller(ControllerKind::Pids)
    }

    /// Reports whether `pids` can be enabled on this cgroup.
    pub fn has_available_pids_controller(&self) -> bool {
        self.has_available_controller(ControllerKind::Pids)
    }

    /// Reports whether this cgroup has live cpuset state.
    pub fn has_cpuset_controller(&self) -> bool {
        self.has_controller(ControllerKind::Cpuset)
    }

    /// Reports whether `cpuset` can be enabled on this cgroup.
    pub fn has_available_cpuset_controller(&self) -> bool {
        self.has_available_controller(ControllerKind::Cpuset)
    }

    /// Reports whether this cgroup delegates `kind` to its children.
    pub fn controller_subtree_enabled(&self, kind: ControllerKind) -> bool {
        self.controllers.is_subtree_enabled(kind)
    }

    /// Reports whether `kind` is implemented and can be enabled here.
    ///
    /// The root always has every implemented controller available; other
    /// cgroups only expose a controller once their parent enabled it.
    pub fn has_available_controller(&self, kind: ControllerKind) -> bool {
        kind.is_implemented() && (self.name.is_empty() || self.controllers.is_active(kind))
    }

    /// Returns implemented controllers available in this cgroup.
    pub fn available_controllers(&self) -> Vec<ControllerKind> {
        ControllerKind::ALL
            .into_iter()
            .filter(|kind| self.has_available_controller(*kind))
            .collect()
    }

    /// Returns controllers whose state is live on this cgroup.
    pub fn active_controllers(&self) -> Vec<ControllerKind> {
        ControllerKind::ALL
            .into_iter()
            .filter(|kind| self.controllers.is_active(*kind))
            .collect()
    }

    /// Returns controllers delegated to this cgroup's children.
    pub fn delegated_controllers(&self) -> Vec<ControllerKind> {
        ControllerKind::ALL
            .into_iter()
            .filter(|kind| self.controllers.is_subtree_enabled(*kind))
            .collect()
    }

    /// Reports whether any controller is delegated to this cgroup's children.
    pub(crate) fn has_delegated_controllers(&self) -> bool {
        ControllerKind::ALL
            .into_iter()
            .any(|kind| self.controllers.is_subtree_enabled(kind))
    }

    /// Enables or disables a controller for this cgroup's direct children.
    ///
    /// # Errors
    ///
    /// - `ENOENT` when this cgroup is not active.
    /// - `EOPNOTSUPP` for a controller whose enforcement hooks are not built.
    /// - `EINVAL` when the controller is not available here.
    /// - `EBUSY` when a non-root cgroup still has member tasks, or when a
    ///   child still delegates the controller.
    pub fn set_subtree_control(&self, kind: ControllerKind, enabled: bool) -> KResult<bool> {
        let _transaction = self.hierarchy.transaction.lock();
        self.set_subtree_control_locked(kind, enabled)
    }

    /// Applies several subtree-control changes under one hierarchy transaction.
    ///
    /// Every operation is validated before the first mutation, so a rejected
    /// write cannot leave a prefix of controllers enabled or reset a child's
    /// controller state through a rollback.
    pub fn set_subtree_controls(&self, operations: &[(ControllerKind, bool)]) -> KResult<()> {
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        // Linux applies duplicate tokens last-token-wins. Dedup before any
        // mutation so a disable/enable pair does not tear down child state.
        let mut resolved: alloc::vec::Vec<(ControllerKind, bool)> =
            alloc::vec::Vec::with_capacity(operations.len());
        for &(kind, enabled) in operations {
            if let Some(existing) = resolved.iter_mut().find(|(known, _)| *known == kind) {
                existing.1 = enabled;
            } else {
                resolved.push((kind, enabled));
            }
        }
        for &(kind, enabled) in &resolved {
            self.validate_subtree_control(kind, enabled)?;
        }
        for &(kind, enabled) in &resolved {
            let _ = self.set_subtree_control_locked(kind, enabled)?;
        }
        Ok(())
    }

    fn validate_subtree_control(&self, kind: ControllerKind, enabled: bool) -> KResult<()> {
        if !kind.is_implemented() {
            return Err(KError::from(LinuxError::EOPNOTSUPP));
        }
        if self.controllers.is_subtree_enabled(kind) == enabled {
            return Ok(());
        }
        if enabled {
            if !self.has_available_controller(kind) {
                return Err(KError::from(LinuxError::ENOENT));
            }
            if !self.name.is_empty() && !self.member_tasks.lock().is_empty() {
                return Err(KError::from(LinuxError::EBUSY));
            }
            if crate::controller::registration(kind).is_none() {
                return Err(KError::from(LinuxError::EOPNOTSUPP));
            }
        } else {
            let children = self.children.read();
            if children
                .values()
                .any(|child| child.controllers.is_subtree_enabled(kind))
            {
                return Err(KError::from(LinuxError::EBUSY));
            }
        }
        Ok(())
    }

    fn set_subtree_control_locked(&self, kind: ControllerKind, enabled: bool) -> KResult<bool> {
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        if !kind.is_implemented() {
            return Err(KError::from(LinuxError::EOPNOTSUPP));
        }
        if self.controllers.is_subtree_enabled(kind) == enabled {
            return Ok(false);
        }
        if enabled {
            self.enable_subtree_control(kind)?;
        } else {
            self.disable_subtree_control(kind)?;
        }
        self.controllers.store_subtree_enabled(kind, enabled);
        Ok(true)
    }

    fn enable_subtree_control(&self, kind: ControllerKind) -> KResult<()> {
        if !self.has_available_controller(kind) {
            return Err(KError::from(LinuxError::ENOENT));
        }
        // Linux's no-internal-process rule: a non-root cgroup with member
        // tasks cannot become an interior node for the controller. Tasks in
        // descendants are unrelated to this cgroup's own membership.
        if !self.name.is_empty() && !self.member_tasks.lock().is_empty() {
            return Err(KError::from(LinuxError::EBUSY));
        }
        let registration = crate::controller::registration(kind)
            .ok_or_else(|| KError::from(LinuxError::EOPNOTSUPP))?;
        let cpus = self.effective_cpus();
        let mems = self.effective_mems();
        let memcg = self.effective_memcg();
        let children: Vec<Arc<Self>> = self.children.read().values().cloned().collect();
        for child in children {
            if child.controllers.is_active(kind) {
                continue;
            }
            let anchor = ControllerAnchor {
                subtree_tasks: child.subtree_task_count(),
                cpus,
                mems,
                memcg: memcg.clone(),
                memory_node: child.memory_node.lock().clone(),
            };
            child
                .controllers
                .activate(kind, registration.activate(anchor));
            if kind == ControllerKind::Memory
                && let Some(controller) = child.controllers.memory()
            {
                *child.memory_node.lock() = Some(controller.memcg());
            }
        }
        Ok(())
    }

    fn disable_subtree_control(&self, kind: ControllerKind) -> KResult<()> {
        let children: Vec<Arc<Self>> = self.children.read().values().cloned().collect();
        if children
            .iter()
            .any(|child| child.controllers.is_subtree_enabled(kind))
        {
            return Err(KError::from(LinuxError::EBUSY));
        }
        for child in &children {
            if kind == ControllerKind::Memory
                && let Some(controller) = child.controllers.memory()
            {
                *child.memory_node.lock() = Some(controller.memcg());
            }
            if kind == ControllerKind::Cpu
                && let Some(controller) = child.controllers.cpu()
            {
                crate::release_bandwidth(&controller.bandwidth());
            }
            child.controllers.deactivate(kind);
        }
        // Disabling delegation only withdraws the controller from this cgroup's
        // children. The controller remains live here because it is still
        // delegated by our parent (or is the root's always-live cpuset).
        Ok(())
    }

    /// Returns the numeric `pids.max`, or `None` when unlimited.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when this cgroup has no live pids controller.
    pub fn pids_max(&self) -> KResult<Option<usize>> {
        Ok(self.pids_controller()?.max())
    }

    /// Stores the numeric `pids.max` or clears it with `None`.
    ///
    /// # Errors
    ///
    /// Returns `ENODEV` when this cgroup is no longer active, `EINVAL` above
    /// [`PIDS_MAX_LIMIT`], and `ENOENT` when the controller is not live.
    pub fn set_pids_max(&self, max: Option<usize>) -> KResult<()> {
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENODEV));
        }
        let controller = self.pids_controller()?;
        controller.set_max(max)
    }

    /// Returns the number of tasks charged to this cgroup's pids controller.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when this cgroup has no live pids controller.
    pub fn pids_current(&self) -> KResult<usize> {
        Ok(self.pids_controller()?.current())
    }

    /// Returns how often new-task admission was rejected by `pids.max`.
    ///
    /// Migration ignores the limit and does not affect this count. The count
    /// is local to this cgroup's live controller and starts at zero whenever
    /// the controller is enabled.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when this cgroup has no live pids controller.
    pub fn pids_max_hits(&self) -> KResult<usize> {
        Ok(self.pids_controller()?.max_hits())
    }

    /// Reports whether this cgroup delegates pids to its children.
    pub fn pids_subtree_enabled(&self) -> bool {
        self.controllers.is_subtree_enabled(ControllerKind::Pids)
    }

    /// Enables or disables pids delegation for direct children.
    ///
    /// # Errors
    ///
    /// See [`set_subtree_control`](Self::set_subtree_control).
    pub fn set_pids_subtree_enabled(&self, enabled: bool) -> KResult<()> {
        self.set_subtree_control(ControllerKind::Pids, enabled)
            .map(|_| ())
    }

    /// Returns the live cpu bandwidth tracker of this cgroup itself, if the
    /// cpu controller is enabled here.
    ///
    /// cgroup v2 `cpu.max` is non-hierarchical: a task bills its own cgroup's
    /// tracker, and the no-internal-process rule guarantees every
    /// task-bearing cgroup owns one once the controller is enabled.
    pub fn cpu_bandwidth(&self) -> Option<alloc::sync::Arc<kbandwidth::Bandwidth>> {
        self.controllers
            .cpu()
            .map(|controller| controller.bandwidth())
    }

    /// Returns the nearest live cpu bandwidth tracker, including ancestors.
    pub fn effective_cpu_bandwidth(&self) -> Option<alloc::sync::Arc<kbandwidth::Bandwidth>> {
        if let Some(controller) = self.controllers.cpu() {
            return Some(controller.bandwidth());
        }
        let mut current = self.parent();
        while let Some(node) = current {
            if let Some(controller) = node.controllers.cpu() {
                return Some(controller.bandwidth());
            }
            current = node.parent();
        }
        None
    }

    /// Returns the nearest live cpu weight, including ancestors.
    pub fn effective_cpu_weight(&self) -> u32 {
        if let Some(controller) = self.controllers.cpu() {
            return controller.weight();
        }
        let mut current = self.parent();
        while let Some(node) = current {
            if let Some(controller) = node.controllers.cpu() {
                return controller.weight();
            }
            current = node.parent();
        }
        100
    }

    /// Returns the cpu controller owned by this cgroup itself.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when this cgroup has no live cpu controller.
    pub fn cpu_controller(&self) -> KResult<Arc<CpuController>> {
        self.controllers
            .cpu()
            .ok_or_else(|| KError::from(LinuxError::ENOENT))
    }

    /// Returns the cpu controller of the nearest ancestor with one live.
    ///
    /// Relative CPU shares are enforced where the task runs, so a member of a
    /// cgroup without its own cpu controller inherits the closest ancestor's
    /// weight. The hierarchy root always owns one once the controller is
    /// enabled anywhere.
    pub fn effective_cpu_controller(&self) -> KResult<Arc<CpuController>> {
        if let Some(controller) = self.controllers.cpu() {
            return Ok(controller);
        }
        let mut current = self.parent.upgrade();
        while let Some(cgroup) = current {
            if let Some(controller) = cgroup.controllers.cpu() {
                return Ok(controller);
            }
            current = cgroup.parent.upgrade();
        }
        Err(KError::from(LinuxError::ENOENT))
    }

    fn pids_controller(&self) -> KResult<Arc<PidsController>> {
        self.controllers
            .pids()
            .ok_or_else(|| KError::from(LinuxError::ENOENT))
    }

    /// Returns the memory controller owned by this cgroup itself.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when this cgroup has no live memory controller.
    pub fn memory_controller(&self) -> KResult<Arc<MemoryController>> {
        self.controllers
            .memory()
            .ok_or_else(|| KError::from(LinuxError::ENOENT))
    }

    /// Returns the nearest live memory accounting node for this cgroup.
    ///
    /// A cgroup without its own memory controller inherits its closest
    /// ancestor's node; the hierarchy root always owns one.
    pub fn effective_memcg(&self) -> Arc<Memcg> {
        if let Some(controller) = self.controllers.memory() {
            return controller.memcg();
        }
        let mut current = self.parent.upgrade();
        while let Some(cgroup) = current {
            if let Some(controller) = cgroup.controllers.memory() {
                return controller.memcg();
            }
            current = cgroup.parent.upgrade();
        }
        memacct::root()
    }

    /// Returns the requested `cpuset.cpus` mask; empty means "inherit".
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when this cgroup has no live cpuset controller.
    pub fn cpuset_cpus(&self) -> KResult<CpuMask> {
        Ok(self.cpuset_controller()?.cpus())
    }

    /// Returns the effective CPU mask enforced for this cgroup.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when this cgroup has no live cpuset controller.
    pub fn cpuset_effective_cpus(&self) -> KResult<CpuMask> {
        Ok(self.cpuset_controller()?.effective_cpus())
    }

    /// Returns the requested `cpuset.mems` mask; empty means "inherit".
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when this cgroup has no live cpuset controller.
    pub fn cpuset_mems(&self) -> KResult<NodeMask> {
        Ok(self.cpuset_controller()?.mems())
    }

    /// Restores effective cpuset masks captured before a transactional write.
    pub fn restore_cpuset_effective(&self, cpus: CpuMask, mems: NodeMask) {
        if let Ok(controller) = self.cpuset_controller() {
            controller.restore_effective(cpus, mems);
        }
    }

    /// Returns the effective NUMA-node mask for this cgroup.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` when this cgroup has no live cpuset controller.
    pub fn cpuset_effective_mems(&self) -> KResult<NodeMask> {
        Ok(self.cpuset_controller()?.effective_mems())
    }

    /// Reports whether this cgroup delegates cpuset to its children.
    pub fn cpuset_subtree_enabled(&self) -> bool {
        self.controllers.is_subtree_enabled(ControllerKind::Cpuset)
    }

    /// Enables or disables cpuset delegation for direct children.
    ///
    /// # Errors
    ///
    /// See [`set_subtree_control`](Self::set_subtree_control).
    pub fn set_cpuset_subtree_enabled(&self, enabled: bool) -> KResult<()> {
        self.set_subtree_control(ControllerKind::Cpuset, enabled)
            .map(|_| ())
    }

    /// Sets `cpuset.cpus` and refreshes the effective masks of the subtree.
    ///
    /// # Errors
    ///
    /// - `ENOENT` when the cgroup or its cpuset controller is not live.
    /// - `ENODEV` when the mask names a CPU that is not present.
    /// - `EINVAL` when the mask is not a subset of the parent's effective mask.
    /// - `ENOSPC` when the resulting effective mask would be empty.
    pub fn set_cpuset_cpus(&self, cpus: CpuMask) -> KResult<()> {
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        let controller = self.cpuset_controller()?;
        let universe = self.hierarchy.cpuset_universe();
        if !cpus.is_subset(&universe) {
            return Err(KError::from(LinuxError::ENODEV));
        }
        let parent_effective = self.parent_effective_cpus();
        if !cpus.is_subset(&parent_effective) {
            return Err(KError::from(LinuxError::EINVAL));
        }
        if cpus.is_empty() && parent_effective.is_empty() {
            return Err(KError::from(LinuxError::ENOSPC));
        }
        controller.set_cpus(cpus, parent_effective);
        self.refresh_cpuset_descendants();
        Ok(())
    }

    /// Sets `cpuset.mems` and refreshes the effective masks of the subtree.
    ///
    /// # Errors
    ///
    /// Mirrors [`set_cpuset_cpus`](Self::set_cpuset_cpus), with `ENODEV` for a
    /// NUMA node that is not present.
    pub fn set_cpuset_mems(&self, mems: NodeMask) -> KResult<()> {
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        let controller = self.cpuset_controller()?;
        let universe = self.hierarchy.mems_universe();
        if !mems.is_subset(&universe) {
            return Err(KError::from(LinuxError::ENODEV));
        }
        let parent_effective = self.parent_effective_mems();
        if !mems.is_subset(&parent_effective) {
            return Err(KError::from(LinuxError::EINVAL));
        }
        if mems.is_empty() && parent_effective.is_empty() {
            return Err(KError::from(LinuxError::ENOSPC));
        }
        controller.set_mems(mems, parent_effective);
        self.refresh_cpuset_descendants();
        Ok(())
    }

    fn cpuset_controller(&self) -> KResult<Arc<CpusetController>> {
        self.controllers
            .cpuset()
            .ok_or_else(|| KError::from(LinuxError::ENOENT))
    }

    fn parent_effective_cpus(&self) -> CpuMask {
        match self.parent() {
            None => self.hierarchy.cpuset_universe(),
            Some(parent) => parent.effective_cpus(),
        }
    }

    fn parent_effective_mems(&self) -> NodeMask {
        match self.parent() {
            None => self.hierarchy.mems_universe(),
            Some(parent) => parent.effective_mems(),
        }
    }

    fn effective_cpus(&self) -> CpuMask {
        self.controllers
            .cpuset()
            .map(|controller| controller.effective_cpus())
            .unwrap_or_else(|| self.hierarchy.cpuset_universe())
    }

    fn effective_mems(&self) -> NodeMask {
        self.controllers
            .cpuset()
            .map(|controller| controller.effective_mems())
            .unwrap_or_else(|| self.hierarchy.mems_universe())
    }

    fn refresh_cpuset_descendants(&self) {
        let cpus = self.effective_cpus();
        let mems = self.effective_mems();
        let children: Vec<Arc<Self>> = self.children.read().values().cloned().collect();
        for child in children {
            if let Some(controller) = child.controllers.cpuset() {
                controller.refresh_effective(cpus, mems);
            }
            child.refresh_cpuset_descendants();
        }
    }

    /// Reserves one task charge against this cgroup.
    ///
    /// # Errors
    ///
    /// Returns `ENOENT` for an inactive cgroup and propagates controller
    /// admission errors (`EAGAIN` for `pids.max`). A failed reservation leaves
    /// no charge behind.
    pub fn reserve_task(self: &Arc<Self>) -> KResult<TaskCharge> {
        let _transaction = self.hierarchy.transaction.lock();
        if !self.is_active() {
            return Err(KError::from(LinuxError::ENOENT));
        }
        let controllers = self.controller_lineage();
        TaskCharge::reserve(self.clone(), &controllers)
    }
}

/// Returns the machine's present CPUs and NUMA nodes.
fn default_universe() -> (CpuMask, NodeMask) {
    let mut cpus = CpuMask::empty();
    let mut found = false;
    kcpu_id_map::for_each_present_logical_cpu(|_, logical_cpu_id, _| {
        if cpus.insert(logical_cpu_id.as_usize()) {
            found = true;
        }
    });
    if !found {
        // Before the CPU map exists only the boot CPU is known; the root
        // universe is refreshed once the present map is published.
        cpus.insert(0);
    }
    let mut mems = NodeMask::empty();
    // Only node 0 is represented until NUMA topology is published.
    mems.insert(0);
    (cpus, mems)
}
