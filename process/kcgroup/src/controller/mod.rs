// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Closed-set controller framework for the unified cgroup v2 hierarchy.
//!
//! Every controller is either **inactive** (its parameters and counters do not
//! exist and no control files may be exposed) or **active** (it owns live state
//! for one cgroup). Activation and deactivation are driven exclusively by
//! `cgroup.subtree_control` writes on the parent, so the visible control files
//! and [`Cgroup::controllers`](crate::Cgroup::controllers) always agree.
//!
//! Controllers that need scheduling, block-layer, or memory-accounting hooks
//! that live outside this crate are listed in [`ControllerKind`] but report
//! [`ControllerKind::is_implemented`] as `false`. They never appear in
//! `cgroup.controllers` and never materialize control files, because a
//! readable-but-inert file would silently drop the guest's resource limits.

use alloc::{sync::Arc, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use kspin::SpinNoIrq;
use memacct::Memcg;

pub(crate) mod cpu;
mod cpuset;
mod memory;
mod pids;

pub use cpu::CpuController;
pub use cpuset::{CpuMask, CpusetController, MAX_CPUS, NodeMask};
pub use memory::MemoryController;
pub use pids::{PIDS_MAX_LIMIT, PidsController};

/// The closed set of cgroup v2 controllers known to this kernel.
///
/// The set is closed on purpose: callers match on it exhaustively, and adding a
/// controller is a compile-time change rather than a string-keyed registration.
#[derive(Clone, Copy, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub enum ControllerKind {
    /// Process-count admission control (`pids.*`).
    Pids,
    /// CPU bandwidth and weight control (`cpu.*`).
    Cpu,
    /// CPU and NUMA-node placement (`cpuset.*`).
    Cpuset,
    /// Memory accounting and reclaim (`memory.*`).
    Memory,
    /// Block I/O bandwidth and weight control (`io.*`).
    Io,
}

impl ControllerKind {
    /// All kinds, in the order used by `cgroup.controllers`.
    pub const ALL: [Self; 5] = [Self::Pids, Self::Cpu, Self::Cpuset, Self::Memory, Self::Io];

    /// Returns the controller name used in `cgroup.controllers` and file names.
    pub const fn name(self) -> &'static str {
        match self {
            Self::Pids => "pids",
            Self::Cpu => "cpu",
            Self::Cpuset => "cpuset",
            Self::Memory => "memory",
            Self::Io => "io",
        }
    }

    /// Reports whether this build registers the controller's semantics.
    ///
    /// `cpu`, `memory`, and `io` require hooks owned by the scheduler, memory
    /// accounting, and block layers. Until those hooks exist, the controllers
    /// are absent from `cgroup.controllers` instead of exposing inert files.
    /// Availability derives from [`registration`], so implementing a
    /// controller is a single registration-table change.
    pub fn is_implemented(self) -> bool {
        registration(self).is_some()
    }

    /// Returns the dense index of this kind in [`Self::ALL`].
    pub const fn index(self) -> usize {
        match self {
            Self::Pids => 0,
            Self::Cpu => 1,
            Self::Cpuset => 2,
            Self::Memory => 3,
            Self::Io => 4,
        }
    }

    /// Parses a controller name as written in `cgroup.subtree_control`.
    ///
    /// Returns `None` for names outside the closed set. Names of controllers
    /// that exist but are not implemented are returned so the caller can report
    /// `EOPNOTSUPP` rather than pretending the controller does not exist.
    pub fn from_name(name: &str) -> Option<Self> {
        match name {
            "pids" => Some(Self::Pids),
            "cpu" => Some(Self::Cpu),
            "cpuset" => Some(Self::Cpuset),
            "memory" => Some(Self::Memory),
            "io" => Some(Self::Io),
            _ => None,
        }
    }
}

/// One controller charge that is released unless it is committed.
///
/// A guard owns the controller it charged, so it can roll the charge back
/// after the caller has dropped its own reference to the cgroup. Guards are
/// produced by [`Controller::fork_charge`] and consumed by
/// [`ChargeSet::commit`].
pub struct ChargeGuard {
    controller: Arc<dyn Controller>,
    count: usize,
    is_armed: bool,
}

impl ChargeGuard {
    pub(crate) fn new(controller: Arc<dyn Controller>, count: usize) -> Self {
        Self {
            controller,
            count,
            is_armed: true,
        }
    }

    /// Commits this charge so dropping the guard no longer releases it.
    pub(crate) fn commit(mut self) {
        self.is_armed = false;
    }
}

impl Drop for ChargeGuard {
    fn drop(&mut self) {
        if self.is_armed {
            self.controller.migrate_out(self.count);
        }
    }
}

/// Ordered controller charges owned by one pending task creation.
///
/// A set is committed as a unit: either every controller keeps its charge or
/// dropping the set releases all of them in reverse charge order.
pub struct ChargeSet {
    guards: Vec<ChargeGuard>,
}

impl ChargeSet {
    /// Creates an empty charge set.
    pub fn new() -> Self {
        Self { guards: Vec::new() }
    }

    /// Appends an already-taken charge.
    pub(crate) fn push(&mut self, guard: ChargeGuard) {
        self.guards.push(guard);
    }

    /// Reports how many controller charges the set owns.
    pub fn len(&self) -> usize {
        self.guards.len()
    }

    /// Reports whether the set owns no charges.
    pub fn is_empty(&self) -> bool {
        self.guards.is_empty()
    }

    /// Reports whether this set still names the exact controller instances.
    pub(crate) fn matches_lineage(&self, lineage: &[Arc<dyn Controller>]) -> bool {
        self.guards.len() == lineage.len()
            && self
                .guards
                .iter()
                .zip(lineage)
                .all(|(guard, controller)| Arc::ptr_eq(&guard.controller, controller))
    }

    /// Commits every charge in the set.
    pub fn commit(self) {
        for guard in self.guards {
            guard.commit();
        }
    }
}

impl Default for ChargeSet {
    fn default() -> Self {
        Self::new()
    }
}

/// Controller behavior shared by every concrete controller.
///
/// Implementations are per-cgroup: one instance exists for each cgroup whose
/// parent enabled the controller in its `cgroup.subtree_control`. Charge hooks
/// are called with the hierarchy transaction held, so they must not sleep,
/// allocate unboundedly, or take runqueue or I/O locks.
///
/// The hooks deliberately deal in cardinality, not task identity. Controller
/// accounting is a counter; identity-dependent enforcement (CPU placement,
/// memory ownership, block-I/O attribution) is applied by `kprocess` after the
/// membership transaction commits, where the live `KtaskRef` is available.
pub trait Controller: Send + Sync {
    /// Returns the closed-set kind of this controller.
    fn kind(&self) -> ControllerKind;

    /// Charges one newly forked task against the controller.
    ///
    /// Called while a [`TaskCharge`](crate::TaskCharge) is being reserved, so
    /// a failure must leave the controller unchanged. The caller wraps a
    /// successful charge in a [`ChargeGuard`], which owns the controller and
    /// releases the charge unless it is committed.
    fn note_fork_failure(&self) {}

    fn fork_charge(&self) -> kerrno::KResult<()> {
        Ok(())
    }

    /// Charges `count` tasks migrated in from another cgroup.
    ///
    /// Migration charges ignore admission limits that only apply to new tasks,
    /// matching Linux: moving a task into a full cgroup must succeed so that a
    /// delegated subtree can always be drained.
    ///
    /// # Errors
    ///
    /// Returns an error if the counter would overflow; a failure must leave
    /// the controller unchanged.
    fn migrate_in(&self, _count: usize) -> kerrno::KResult<()> {
        Ok(())
    }

    /// Releases `count` previously charged tasks.
    fn migrate_out(&self, _count: usize) {}

    /// Clears all live state because the controller was disabled.
    ///
    /// The controller is already unreachable from the hierarchy when this is
    /// called; implementations only need to drop or reset their own counters.
    fn deactivate(&self) {}
}

/// Activation state of one controller slot.
pub enum ControllerState<T> {
    /// No controller state exists and no control files are visible.
    Inactive,
    /// The controller is live and owns state for this cgroup.
    Active(T),
}

impl<T> ControllerState<T> {
    /// Reports whether the slot holds live controller state.
    pub fn is_active(&self) -> bool {
        matches!(self, Self::Active(_))
    }

    /// Returns the live state, if any.
    pub fn active(&self) -> Option<&T> {
        match self {
            Self::Inactive => None,
            Self::Active(value) => Some(value),
        }
    }
}

/// One controller slot: live state plus the subtree-delegation flag.
///
/// The flag lives outside the state lock because every control-file read
/// consults it, while activation only happens on `cgroup.subtree_control`
/// writes. `subtree_enabled` is meaningful for inactive slots too: it records
/// that children created from now on must receive an active controller.
pub struct ControllerSlot<T> {
    state: SpinNoIrq<ControllerState<T>>,
    subtree_enabled: AtomicBool,
}

impl<T> ControllerSlot<T> {
    /// Creates an inactive slot with delegation disabled.
    pub fn new() -> Self {
        Self {
            state: SpinNoIrq::new(ControllerState::Inactive),
            subtree_enabled: AtomicBool::new(false),
        }
    }

    /// Reports whether the slot holds live controller state.
    pub fn is_active(&self) -> bool {
        self.state.lock().is_active()
    }

    /// Returns a clone of the live state, if any.
    pub fn active(&self) -> Option<T>
    where
        T: Clone,
    {
        self.state.lock().active().cloned()
    }

    /// Installs live state, replacing any previous state.
    pub fn activate(&self, value: T) {
        *self.state.lock() = ControllerState::Active(value);
    }

    /// Removes and discards live state.
    pub fn deactivate(&self) {
        *self.state.lock() = ControllerState::Inactive;
    }

    /// Reports whether this cgroup delegates the controller to its children.
    pub fn is_subtree_enabled(&self) -> bool {
        self.subtree_enabled.load(Ordering::Acquire)
    }

    /// Stores the delegation flag and returns its previous value.
    pub fn store_subtree_enabled(&self, enabled: bool) -> bool {
        self.subtree_enabled.swap(enabled, Ordering::AcqRel)
    }
}

impl<T> Default for ControllerSlot<T> {
    fn default() -> Self {
        Self::new()
    }
}

/// One live controller instance.
///
/// The enum keeps typed accessors (used by control files) next to the
/// trait-object view (used by charge and migration hooks) without exposing
/// activation of mismatched types. Adding a controller adds one variant and
/// one arm in [`ControllerInstance::kind`]; no `ControllerSet` accessor needs
/// a per-kind "unsupported" stub.
#[derive(Clone)]
pub enum ControllerInstance {
    /// Process-count controller.
    Pids(Arc<PidsController>),
    /// CPU weight and bandwidth controller.
    Cpu(Arc<CpuController>),
    /// CPU placement controller.
    Cpuset(Arc<CpusetController>),
    /// Memory accounting controller.
    Memory(Arc<MemoryController>),
}

impl ControllerInstance {
    /// Returns the concrete kind of this instance.
    pub fn kind(&self) -> ControllerKind {
        match self {
            Self::Pids(_) => ControllerKind::Pids,
            Self::Cpu(_) => ControllerKind::Cpu,
            Self::Cpuset(_) => ControllerKind::Cpuset,
            Self::Memory(_) => ControllerKind::Memory,
        }
    }

    /// Returns the pids controller when this is a pids instance.
    pub fn pids(&self) -> Option<Arc<PidsController>> {
        match self {
            Self::Pids(controller) => Some(controller.clone()),
            Self::Cpu(_) | Self::Cpuset(_) | Self::Memory(_) => None,
        }
    }

    /// Returns the cpuset controller when this is a cpuset instance.
    pub fn cpuset(&self) -> Option<Arc<CpusetController>> {
        match self {
            Self::Cpuset(controller) => Some(controller.clone()),
            Self::Pids(_) | Self::Cpu(_) | Self::Memory(_) => None,
        }
    }

    /// Returns the cpu controller when this is a cpu instance.
    pub fn cpu(&self) -> Option<Arc<CpuController>> {
        match self {
            Self::Cpu(controller) => Some(controller.clone()),
            Self::Pids(_) | Self::Cpuset(_) | Self::Memory(_) => None,
        }
    }

    /// Returns the memory controller when this is a memory instance.
    pub fn memory(&self) -> Option<Arc<MemoryController>> {
        match self {
            Self::Memory(controller) => Some(controller.clone()),
            Self::Pids(_) | Self::Cpu(_) | Self::Cpuset(_) => None,
        }
    }
}

impl Controller for ControllerInstance {
    fn kind(&self) -> ControllerKind {
        Self::kind(self)
    }

    fn note_fork_failure(&self) {
        match self {
            Self::Pids(controller) => controller.note_fork_failure(),
            Self::Cpu(controller) => controller.note_fork_failure(),
            Self::Cpuset(controller) => controller.note_fork_failure(),
            Self::Memory(controller) => controller.note_fork_failure(),
        }
    }

    fn fork_charge(&self) -> kerrno::KResult<()> {
        match self {
            Self::Pids(controller) => controller.fork_charge(),
            Self::Cpu(controller) => controller.fork_charge(),
            Self::Cpuset(controller) => controller.fork_charge(),
            Self::Memory(controller) => controller.fork_charge(),
        }
    }

    fn migrate_in(&self, count: usize) -> kerrno::KResult<()> {
        match self {
            Self::Pids(controller) => controller.migrate_in(count),
            Self::Cpu(controller) => controller.migrate_in(count),
            Self::Cpuset(controller) => controller.migrate_in(count),
            Self::Memory(controller) => controller.migrate_in(count),
        }
    }

    fn migrate_out(&self, count: usize) {
        match self {
            Self::Pids(controller) => controller.migrate_out(count),
            Self::Cpu(controller) => controller.migrate_out(count),
            Self::Cpuset(controller) => controller.migrate_out(count),
            Self::Memory(controller) => controller.migrate_out(count),
        }
    }

    fn deactivate(&self) {
        match self {
            Self::Pids(controller) => controller.deactivate(),
            Self::Cpu(controller) => controller.deactivate(),
            Self::Cpuset(controller) => controller.deactivate(),
            Self::Memory(controller) => controller.deactivate(),
        }
    }
}

/// Inputs a controller needs when it becomes live for a child cgroup.
///
/// The anchor is captured by the parent while it holds the hierarchy
/// transaction. `cpus`/`mems` are the parent's effective masks, which is what
/// a fresh child's cpuset uses as its inherited placement. `memcg` is the
/// memory cgroup the child's own node must be parented to.
#[derive(Clone)]
pub(crate) struct ControllerAnchor {
    /// Number of tasks already charged to the child subtree.
    pub subtree_tasks: usize,
    /// Parent's effective CPU mask.
    pub cpus: CpuMask,
    /// Parent's effective NUMA-node mask.
    pub mems: NodeMask,
    /// Parent's effective memory cgroup.
    pub memcg: Arc<Memcg>,
    /// Existing accounting node when re-enabling a controller on this child.
    pub memory_node: Option<Arc<Memcg>>,
}

/// Builds the initial live state for a controller on one child cgroup.
pub(crate) type ControllerFactory = fn(ControllerAnchor) -> ControllerInstance;

/// One implemented controller and its activation constructor.
pub(crate) struct ControllerRegistration {
    kind: ControllerKind,
    activate: ControllerFactory,
}

/// Controllers implemented by this build.
///
/// This table is the single source of truth for `cgroup.controllers`,
/// `cgroup.subtree_control`, and child activation. A controller whose
/// enforcement hooks are not built must not be listed here: exposing its
/// control files without enforcement would silently drop guest limits.
const CONTROLLERS: &[ControllerRegistration] = &[
    ControllerRegistration {
        kind: ControllerKind::Pids,
        activate: |anchor| {
            ControllerInstance::Pids(Arc::new(PidsController::new(anchor.subtree_tasks)))
        },
    },
    ControllerRegistration {
        kind: ControllerKind::Cpu,
        activate: |_anchor| ControllerInstance::Cpu(Arc::new(CpuController::new())),
    },
    ControllerRegistration {
        kind: ControllerKind::Cpuset,
        activate: |anchor| {
            ControllerInstance::Cpuset(Arc::new(CpusetController::new(anchor.cpus, anchor.mems)))
        },
    },
    ControllerRegistration {
        kind: ControllerKind::Memory,
        activate: |anchor| {
            let memcg = anchor
                .memory_node
                .unwrap_or_else(|| memacct::create_child(&anchor.memcg));
            ControllerInstance::Memory(Arc::new(MemoryController::new(memcg)))
        },
    },
];

/// Returns the registration for `kind`, if this build implements it.
pub(crate) fn registration(kind: ControllerKind) -> Option<&'static ControllerRegistration> {
    CONTROLLERS.iter().find(|entry| entry.kind == kind)
}

/// Returns every implemented controller in `cgroup.controllers` order.
pub(crate) fn registrations() -> &'static [ControllerRegistration] {
    CONTROLLERS
}

impl ControllerRegistration {
    /// Returns the kind this registration constructs.
    pub(crate) fn kind(&self) -> ControllerKind {
        self.kind
    }

    /// Constructs the live state for a child cgroup.
    pub(crate) fn activate(&self, anchor: ControllerAnchor) -> ControllerInstance {
        (self.activate)(anchor)
    }
}

/// All controller slots owned by one cgroup, indexed by [`ControllerKind`].
///
/// Every slot exists in every cgroup, but an inactive slot owns no state and
/// exposes no files. Delegation flags are stored on the slot itself, so a
/// controller can be delegated before any child exists and a controller that
/// is disabled still remembers that its children must not be created with it.
pub struct ControllerSet {
    slots: [ControllerSlot<Arc<ControllerInstance>>; ControllerKind::ALL.len()],
}

impl ControllerSet {
    /// Creates a set with every controller inactive.
    pub fn new() -> Self {
        Self {
            slots: core::array::from_fn(|_| ControllerSlot::new()),
        }
    }

    /// Creates a set whose cpuset and memory slots are already active.
    ///
    /// The hierarchy root always owns both: its cpuset mask is the
    /// machine-wide CPU universe every descendant intersects with, and its
    /// memory node is the ancestor of every delegated memory cgroup.
    pub fn new_root(cpuset: Arc<CpusetController>, memcg: Arc<Memcg>) -> Self {
        let set = Self::new();
        set.activate(ControllerKind::Cpuset, ControllerInstance::Cpuset(cpuset));
        set.activate(
            ControllerKind::Memory,
            ControllerInstance::Memory(Arc::new(MemoryController::new(memcg))),
        );
        set
    }

    fn slot(&self, kind: ControllerKind) -> &ControllerSlot<Arc<ControllerInstance>> {
        &self.slots[kind.index()]
    }

    /// Returns the live pids controller, if enabled.
    pub fn pids(&self) -> Option<Arc<PidsController>> {
        self.slot(ControllerKind::Pids)
            .active()
            .and_then(|controller| controller.pids())
    }

    /// Returns the live cpu controller, if enabled.
    pub fn cpu(&self) -> Option<Arc<CpuController>> {
        self.slot(ControllerKind::Cpu)
            .active()
            .and_then(|controller| controller.cpu())
    }

    /// Returns the live cpuset controller, if enabled.
    pub fn cpuset(&self) -> Option<Arc<CpusetController>> {
        self.slot(ControllerKind::Cpuset)
            .active()
            .and_then(|controller| controller.cpuset())
    }

    /// Returns the live memory controller, if enabled.
    pub fn memory(&self) -> Option<Arc<MemoryController>> {
        self.slot(ControllerKind::Memory)
            .active()
            .and_then(|controller| controller.memory())
    }

    /// Installs `controller` in the slot selected by its kind.
    ///
    /// # Panics
    ///
    /// Panics when `kind` disagrees with the instance. All call sites are
    /// internal and pass a golden pair; keeping the check here prevents a
    /// future caller from installing a pids controller into the cpuset slot.
    pub fn activate(&self, kind: ControllerKind, controller: ControllerInstance) {
        debug_assert_eq!(kind, controller.kind());
        self.slot(kind).activate(Arc::new(controller));
    }

    /// Removes the live state of `kind`, dropping its counters.
    pub fn deactivate(&self, kind: ControllerKind) {
        self.slot(kind).deactivate();
    }

    /// Reports whether `kind` is live on this cgroup.
    pub fn is_active(&self, kind: ControllerKind) -> bool {
        self.slot(kind).is_active()
    }

    /// Returns the live controller for `kind` as a trait object.
    pub fn controller(&self, kind: ControllerKind) -> Option<Arc<dyn Controller>> {
        self.slot(kind)
            .active()
            .map(|controller| controller as Arc<dyn Controller>)
    }

    /// Returns every live controller in [`ControllerKind::ALL`] order.
    pub fn controllers(&self) -> Vec<Arc<dyn Controller>> {
        ControllerKind::ALL
            .into_iter()
            .filter_map(|kind| self.controller(kind))
            .collect()
    }

    /// Reports whether this cgroup delegates `kind` to its children.
    pub fn is_subtree_enabled(&self, kind: ControllerKind) -> bool {
        self.slot(kind).is_subtree_enabled()
    }

    /// Stores the delegation flag for `kind` and returns its previous value.
    pub fn store_subtree_enabled(&self, kind: ControllerKind, enabled: bool) -> bool {
        self.slot(kind).store_subtree_enabled(enabled)
    }
}

impl Default for ControllerSet {
    fn default() -> Self {
        Self::new()
    }
}
