// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Hierarchical memory accounting for memory cgroups.
//!
//! [`Memcg`] owns the usage, protection and event counters of one cgroup.
//! Physical pages record the accounting group that first charged them, so
//! releasing a page does not depend on the releasing task still running in the
//! same cgroup. Controllers live in `kcgroup`; this crate only owns the
//! accounting tree and the page-ownership hand-off.
//!
//! ## Atomic ordering
//!
//! - `usage`, `subtree_usage`, and the stat counters use `AcqRel` updates under
//!   `CHARGE_LOCK`, and `Acquire` reads observe a completed commit.
//! - `peak`/`subtree_peak` use `AcqRel` compare-exchange loops; they are
//!   monotonic high-water marks and never publish another field.
//! - Limit fields use `Release` stores and `Acquire` loads so a limit is
//!   observed with the charging state that justified the update.
//! - Event counters are independent monotonic counters; `Relaxed` increments
//!   are sufficient because they publish no other data.
//! - `dead` uses `Release` to publish controller teardown and `Acquire` to gate
//!   reclamation after the last uncharge.
//! - The provider pointer uses `AcqRel` publication and `Acquire` read; it is
//!   installed once and never replaced.
#![no_std]

extern crate alloc;

use alloc::{collections::BTreeMap, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};

use kerrno::{KError, KResult};
use klazy::Once;
use kspin::{SpinNoIrq, static_lock};
use memaddr::PhysAddr;
use page_meta::{UsageKind, lookup};

const PAGE_SIZE: usize = 4096;
const UNLIMITED: u64 = u64::MAX;

/// Identity of one memory cgroup.
///
/// Zero means that no memory cgroup owns the page or task. Identities are
/// allocated monotonically and are never reused.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemcgId(u32);

impl MemcgId {
    /// The absence of a memory cgroup.
    pub const NONE: Self = Self(0);

    /// Reconstructs an identity previously read from task or page metadata.
    ///
    /// The value must have come from a [`Memcg`] created by this crate.
    pub const fn from_raw(id: u32) -> Self {
        Self(id)
    }

    /// Returns whether no memory cgroup is associated with this value.
    pub const fn is_none(self) -> bool {
        self.0 == 0
    }

    /// Returns the raw identity used by task and page metadata.
    pub const fn as_u32(self) -> u32 {
        self.0
    }
}

/// Page-attribution counters exposed through `memory.stat`.
///
/// Only classes with a real charge source are reported; a category that this
/// kernel cannot distinguish is omitted rather than derived from the total.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryStats {
    /// Anonymous user memory charged through the virtual-memory backends.
    pub anon: u64,
    /// File-backed page-cache memory.
    pub file: u64,
}

/// Hierarchical memory counters exposed through `memory.events`.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct MemoryEvents {
    /// Reserved for a future reclaim path; currently always zero because this
    /// kernel has no memory reclaimer to report a low-boundary failure.
    pub low: u64,
    /// Reserved for a future reclaim path; currently always zero.
    pub high: u64,
    /// Times a charge was rejected by `memory.max`.
    pub max: u64,
    /// No OOM killer is implemented by this crate, so this remains zero.
    pub oom: u64,
    /// No OOM killer is implemented by this crate, so this remains zero.
    pub oom_kill: u64,
}

/// One node of the memory accounting hierarchy.
///
/// A node remains reachable while it owns a page charge. Its controller marks
/// it dead on teardown; the last uncharge reclaims the lookup entry. Identities
/// are 32-bit and never reused.
pub struct Memcg {
    id: MemcgId,
    parent: Option<Arc<Self>>,
    dead: AtomicBool,
    usage: AtomicU64,
    subtree_usage: AtomicU64,
    peak: AtomicU64,
    subtree_peak: AtomicU64,
    max: AtomicU64,
    high: AtomicU64,
    low: AtomicU64,
    swap_max: AtomicU64,
    events: Events,
    subtree_events: Events,
    stats: StatCounters,
    subtree_stats: StatCounters,
}

struct Events {
    low: AtomicU64,
    high: AtomicU64,
    max: AtomicU64,
    oom: AtomicU64,
    oom_kill: AtomicU64,
}

impl Events {
    const fn new() -> Self {
        Self {
            low: AtomicU64::new(0),
            high: AtomicU64::new(0),
            max: AtomicU64::new(0),
            oom: AtomicU64::new(0),
            oom_kill: AtomicU64::new(0),
        }
    }

    fn note(&self, kind: EventKind) {
        let counter = match kind {
            EventKind::Max => &self.max,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }
}

#[derive(Clone, Copy)]
enum ChargeClass {
    Anonymous,
    File,
}

impl ChargeClass {
    fn of(kind: UsageKind) -> Self {
        match kind {
            UsageKind::PageCache => Self::File,
            _ => Self::Anonymous,
        }
    }
}

struct StatCounters {
    anon: AtomicU64,
    file: AtomicU64,
}

impl StatCounters {
    const fn new() -> Self {
        Self {
            anon: AtomicU64::new(0),
            file: AtomicU64::new(0),
        }
    }

    fn counter(&self, class: ChargeClass) -> &AtomicU64 {
        match class {
            ChargeClass::Anonymous => &self.anon,
            ChargeClass::File => &self.file,
        }
    }

    fn add(&self, class: ChargeClass, bytes: u64) {
        self.counter(class).fetch_add(bytes, Ordering::AcqRel);
    }

    fn subtract(&self, class: ChargeClass, bytes: u64) {
        self.counter(class).fetch_sub(bytes, Ordering::AcqRel);
    }

    fn snapshot(&self) -> MemoryStats {
        MemoryStats {
            anon: self.anon.load(Ordering::Acquire),
            file: self.file.load(Ordering::Acquire),
        }
    }
}

#[derive(Clone, Copy)]
enum EventKind {
    Max,
}

/// Records one event on `node` and its ancestors while the charge lock is
/// held, so hierarchical counters stay consistent with the charge rollback.
///
/// The local counter belongs only to the node whose boundary was crossed;
/// ancestors observe the same event through their subtree counter.
fn note_event_locked(node: &Memcg, kind: EventKind) {
    node.events.note(kind);
    note_subtree_event_locked(node, kind);
}

fn note_subtree_event_locked(node: &Memcg, kind: EventKind) {
    node.subtree_events.note(kind);
    if let Some(parent) = &node.parent {
        note_subtree_event_locked(parent, kind);
    }
}

impl Memcg {
    fn new(id: MemcgId, parent: Option<Arc<Self>>) -> Self {
        Self {
            id,
            parent,
            dead: AtomicBool::new(false),
            usage: AtomicU64::new(0),
            subtree_usage: AtomicU64::new(0),
            peak: AtomicU64::new(0),
            subtree_peak: AtomicU64::new(0),
            max: AtomicU64::new(UNLIMITED),
            high: AtomicU64::new(UNLIMITED),
            low: AtomicU64::new(0),
            swap_max: AtomicU64::new(UNLIMITED),
            events: Events::new(),
            subtree_events: Events::new(),
            stats: StatCounters::new(),
            subtree_stats: StatCounters::new(),
        }
    }

    /// Returns this node's stable identity.
    pub fn id(&self) -> MemcgId {
        self.id
    }

    /// Returns the parent node, or `None` for the root.
    pub fn parent(&self) -> Option<&Arc<Self>> {
        self.parent.as_ref()
    }

    /// Returns the charge currently attributed to this node alone.
    pub fn usage(&self) -> u64 {
        self.usage.load(Ordering::Acquire)
    }

    /// Returns the largest charge this node has held.
    pub fn peak(&self) -> u64 {
        self.peak.load(Ordering::Acquire)
    }

    /// Returns the charge of this node and every descendant.
    pub fn hierarchical_usage(&self) -> u64 {
        self.subtree_usage.load(Ordering::Acquire)
    }

    /// Returns the largest hierarchical charge observed.
    pub fn hierarchical_peak(&self) -> u64 {
        self.subtree_peak.load(Ordering::Acquire)
    }

    /// Returns the soft limit of this node, or `None` when unlimited.
    pub fn max(&self) -> Option<u64> {
        decode_limit(self.max.load(Ordering::Acquire))
    }

    /// Sets a hard limit. The new value may be below current usage; further
    /// charges then fail until usage drops.
    pub fn set_max(&self, limit: Option<u64>) {
        self.max.store(encode_limit(limit), Ordering::Release);
    }

    /// Returns the reclaim threshold, or `None` when unlimited.
    pub fn high(&self) -> Option<u64> {
        decode_limit(self.high.load(Ordering::Acquire))
    }

    /// Sets the reclaim threshold. The value is retained for a future reclaim
    /// path; charging never rejects and this kernel does not emit a fabricated
    /// `high` event without an actual reclaim attempt.
    pub fn set_high(&self, limit: Option<u64>) {
        self.high.store(encode_limit(limit), Ordering::Release);
    }

    /// Returns the reclaim-protection floor.
    pub fn low(&self) -> u64 {
        self.low.load(Ordering::Acquire)
    }

    /// Sets the reclaim-protection floor. The value is retained for a future
    /// reclaim path; no `low` event is emitted until reclaim exists.
    pub fn set_low(&self, limit: u64) {
        self.low.store(limit, Ordering::Release);
    }

    /// Returns the swap limit, or `None` when unlimited.
    pub fn swap_max(&self) -> Option<u64> {
        decode_limit(self.swap_max.load(Ordering::Acquire))
    }

    /// Sets the swap limit. This kernel has no swap backend, so only
    /// unlimited and zero are meaningful.
    pub fn set_swap_max(&self, limit: Option<u64>) {
        self.swap_max.store(encode_limit(limit), Ordering::Release);
    }

    /// Returns local event counters.
    pub fn events(&self) -> MemoryEvents {
        MemoryEvents {
            low: self.events.low.load(Ordering::Relaxed),
            high: self.events.high.load(Ordering::Relaxed),
            max: self.events.max.load(Ordering::Relaxed),
            oom: self.events.oom.load(Ordering::Relaxed),
            oom_kill: self.events.oom_kill.load(Ordering::Relaxed),
        }
    }

    /// Returns page-attribution counters for this node alone.
    pub fn stats(&self) -> MemoryStats {
        self.stats.snapshot()
    }

    /// Returns page-attribution counters for this node and all descendants.
    pub fn hierarchical_stats(&self) -> MemoryStats {
        self.subtree_stats.snapshot()
    }

    /// Sets a synthetic peak for reset-path tests.
    #[cfg(unittest)]
    pub fn set_peak_for_test(&self, value: u64) {
        self.peak.store(value, Ordering::Release);
        self.subtree_peak.store(value, Ordering::Release);
    }

    /// Resets the visible peak to the current usage.
    ///
    /// The charge lock excludes concurrent commits, so the new peak is a
    /// coherent snapshot rather than a value already below live usage.
    pub fn reset_peak(&self) {
        let _guard = CHARGE_LOCK.lock();
        self.peak.store(self.usage(), Ordering::Release);
        self.subtree_peak.store(
            self.subtree_usage.load(Ordering::Acquire),
            Ordering::Release,
        );
    }

    /// Returns event counters for this node and all descendants.
    pub fn hierarchical_events(&self) -> MemoryEvents {
        MemoryEvents {
            low: self.subtree_events.low.load(Ordering::Relaxed),
            high: self.subtree_events.high.load(Ordering::Relaxed),
            max: self.subtree_events.max.load(Ordering::Relaxed),
            oom: self.subtree_events.oom.load(Ordering::Relaxed),
            oom_kill: self.subtree_events.oom_kill.load(Ordering::Relaxed),
        }
    }
}

const fn encode_limit(limit: Option<u64>) -> u64 {
    match limit {
        Some(value) => value,
        None => UNLIMITED,
    }
}

const fn decode_limit(raw: u64) -> Option<u64> {
    if raw == UNLIMITED { None } else { Some(raw) }
}

static ROOT: Once<Arc<Memcg>> = Once::new();
static NEXT_ID: AtomicU32 = AtomicU32::new(1);
static_lock! {
    static TREE: SpinNoIrq<BTreeMap<u32, Arc<Memcg>>> = SpinNoIrq::new(BTreeMap::new());
}
static_lock! {
    static CHARGE_LOCK: SpinNoIrq<()> = SpinNoIrq::new(());
}
/// Provider for the memory cgroup that owns a new page allocation.
///
/// The process/cgroup layer installs this after it can resolve the current
/// task's membership. Keeping the lookup behind a function pointer avoids a
/// `memacct -> kprocess -> memacct` dependency cycle and keeps the accounting
/// crate free of scheduler-specific per-CPU state.
pub type CurrentMemcgProvider = fn() -> MemcgId;
static CURRENT_PROVIDER: AtomicUsize = AtomicUsize::new(0);

fn new_node(parent: Option<Arc<Memcg>>) -> Arc<Memcg> {
    let raw = loop {
        let candidate = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        if candidate != 0 {
            break candidate;
        }
    };
    let node = Arc::new(Memcg::new(MemcgId::from_raw(raw), parent));
    TREE.lock().insert(raw, Arc::clone(&node));
    node
}

/// Returns the root accounting node, creating it on first use.
pub fn root() -> Arc<Memcg> {
    if let Some(root) = ROOT.get() {
        return Arc::clone(root);
    }
    Arc::clone(ROOT.call_once(|| new_node(None)))
}

/// Creates a child of `parent`.
pub fn create_child(parent: &Arc<Memcg>) -> Arc<Memcg> {
    new_node(Some(Arc::clone(parent)))
}

/// Looks up a node by stable identity.
pub fn get(id: MemcgId) -> Option<Arc<Memcg>> {
    if id.is_none() {
        return None;
    }
    TREE.lock().get(&id.0).cloned()
}

/// Marks a cgroup node as removable and reclaims it when it owns no pages.
///
/// The process/cgroup layer calls this when the memory controller is detached.
/// A node with live page charges stays in the lookup table until the last
/// charge is released; [`get`] then stops returning it. Identities are never
/// reused, so a late lookup cannot alias a newer cgroup.
pub fn remove(id: MemcgId) {
    if id.is_none() {
        return;
    }
    let Some(node) = get(id) else {
        return;
    };
    // The root is the global accounting fallback for all kernel work; dropping
    // a namespace or test hierarchy must never retire it.
    if node.parent().is_none() {
        return;
    }
    {
        let _guard = CHARGE_LOCK.lock();
        node.dead.store(true, Ordering::Release);
    }
    try_remove_dead(&node);
}

fn try_remove_dead(node: &Memcg) {
    if !node.dead.load(Ordering::Acquire)
        || node.usage.load(Ordering::Acquire) != 0
        || node.subtree_usage.load(Ordering::Acquire) != 0
    {
        return;
    }
    TREE.lock().remove(&node.id.0);
}

/// Installs the process/cgroup layer's current-membership provider.
///
/// The provider is queried for every new page charge, so task migration and
/// context switches are observed without maintaining a separate per-CPU
/// cache. Only one provider may be installed; later calls are ignored.
pub fn install_current_memcg_provider(provider: CurrentMemcgProvider) -> bool {
    CURRENT_PROVIDER
        .compare_exchange(0, provider as usize, Ordering::AcqRel, Ordering::Acquire)
        .is_ok()
}

/// Returns the provider-selected memory cgroup, or [`MemcgId::NONE`] before
/// the process/cgroup layer has installed its provider.
///
/// Execution context: this is callable from page-allocation and interrupt
/// context. The installed provider may inspect the current task and cgroup
/// membership, but must be IRQ-safe: no sleeping locks, allocation, blocking,
/// or page faults. The default before installation is context-independent and
/// returns [`MemcgId::NONE`].
pub fn current_memcg() -> MemcgId {
    let provider = CURRENT_PROVIDER.load(Ordering::Acquire);
    if provider == 0 {
        return MemcgId::NONE;
    }
    // SAFETY: the only writer stores a valid `CurrentMemcgProvider` function
    // pointer and never replaces it; the atomic load above synchronizes with
    // that publication.
    let provider: CurrentMemcgProvider = unsafe { core::mem::transmute(provider) };
    provider()
}

fn record_stats_locked(node: &Memcg, target: &Memcg, bytes: u64, class: ChargeClass) {
    if core::ptr::eq(node, target) {
        node.stats.add(class, bytes);
    }
    node.subtree_stats.add(class, bytes);
}

fn uncharge_locked(node: &Memcg, target: &Memcg, bytes: u64, class: ChargeClass) {
    if core::ptr::eq(node, target) {
        node.usage.fetch_sub(bytes, Ordering::AcqRel);
        node.stats.subtract(class, bytes);
    }
    node.subtree_usage.fetch_sub(bytes, Ordering::AcqRel);
    node.subtree_stats.subtract(class, bytes);
    if let Some(parent) = &node.parent {
        uncharge_locked(parent, target, bytes, class);
    }
}

fn check_limits(node: &Arc<Memcg>, bytes: u64) -> Result<(), Arc<Memcg>> {
    let max = node.max.load(Ordering::Acquire);
    if max != UNLIMITED
        && node
            .subtree_usage
            .load(Ordering::Acquire)
            .saturating_add(bytes)
            > max
    {
        return Err(node.clone());
    }
    if let Some(parent) = &node.parent {
        check_limits(parent, bytes)?;
    }
    Ok(())
}

fn charge_locked(node: &Arc<Memcg>, target: &Arc<Memcg>, bytes: u64, class: ChargeClass) {
    if let Some(parent) = &node.parent {
        charge_locked(parent, target, bytes, class);
    }

    let is_target = Arc::ptr_eq(node, target);
    let previous = if is_target {
        Some(node.usage.fetch_add(bytes, Ordering::AcqRel))
    } else {
        None
    };
    let previous_subtree = node.subtree_usage.fetch_add(bytes, Ordering::AcqRel);
    record_stats_locked(node, target, bytes, class);
    let charged_subtree = previous_subtree.saturating_add(bytes);

    if let Some(previous) = previous {
        let charged = previous.saturating_add(bytes);
        let _ = node
            .peak
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |peak| {
                (charged > peak).then_some(charged)
            });
    }
    let _ = node
        .subtree_peak
        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |peak| {
            (charged_subtree > peak).then_some(charged_subtree)
        });
    // `low`/`high` are thresholds for a future reclaim path. This kernel has
    // no reclaim implementation, so it does not manufacture reclaim-failure
    // events: those counters remain zero until reclaim exists.
}

fn charge_bytes_class(node: &Arc<Memcg>, bytes: u64, class: ChargeClass) -> KResult<()> {
    if bytes == 0 {
        return Ok(());
    }
    {
        let _guard = CHARGE_LOCK.lock();
        if node.dead.load(Ordering::Acquire) {
            return Err(KError::InvalidInput);
        }
        if let Err(failed) = check_limits(node, bytes) {
            note_event_locked(&failed, EventKind::Max);
            return Err(KError::NoMemory);
        }
        charge_locked(node, node, bytes, class);
    }
    Ok(())
}

#[cfg(unittest)]
fn uncharge_bytes(node: &Arc<Memcg>, bytes: u64) {
    uncharge_bytes_class(node, bytes, ChargeClass::Anonymous);
}

#[cfg(unittest)]
fn charge_bytes(node: &Arc<Memcg>, bytes: u64) -> KResult<()> {
    charge_bytes_class(node, bytes, ChargeClass::Anonymous)
}

fn uncharge_bytes_class(node: &Arc<Memcg>, bytes: u64, class: ChargeClass) {
    if bytes == 0 {
        return;
    }
    {
        let _guard = CHARGE_LOCK.lock();
        uncharge_locked(node, node, bytes, class);
    }
    // Reclaim outside CHARGE_LOCK: `charge_frame` takes TREE before
    // CHARGE_LOCK, so taking TREE while holding CHARGE_LOCK would invert the
    // lock order. A dead ancestor may only be held by subtree usage, so walk
    // the parent chain after the leaf uncharge.
    let mut current = Some(node.clone());
    while let Some(candidate) = current {
        try_remove_dead(&candidate);
        current = candidate.parent().cloned();
    }
}

/// Charges `bytes` held by `paddr` to the current memory cgroup.
///
/// The page's metadata records the owner and allocation class so
/// [`uncharge_frame`] can release the charge from an arbitrary later context,
/// including after the allocating task has migrated or exited. Every page in
/// the range must have resident page metadata before any counter changes;
/// otherwise the caller gets `NoMemory` and no partial charge is published.
pub fn charge_frame(paddr: PhysAddr, bytes: usize, kind: UsageKind) -> KResult<()> {
    let id = current_memcg();
    if id.is_none() {
        return Ok(());
    }
    let first_page = paddr.as_usize() / PAGE_SIZE;
    let pages = bytes.div_ceil(PAGE_SIZE);
    for page in 0..pages {
        if lookup(first_page + page).is_none() {
            return Err(KError::NoMemory);
        }
    }
    let Some(node) = get(id) else {
        return Err(KError::InvalidInput);
    };
    let class = ChargeClass::of(kind);
    charge_bytes_class(&node, bytes as u64, class)?;
    for page in 0..pages {
        if let Some(meta) = lookup(first_page + page) {
            meta.set_usage_kind(kind);
            meta.set_memcg(id.as_u32());
        }
    }
    Ok(())
}

/// Releases the charge recorded for `paddr`.
pub fn uncharge_frame(paddr: PhysAddr, bytes: usize) {
    let first_page = paddr.as_usize() / PAGE_SIZE;
    let pages = bytes.div_ceil(PAGE_SIZE);
    let Some(first_meta) = lookup(first_page) else {
        return;
    };
    let class = ChargeClass::of(first_meta.usage_kind());
    let mut owner = MemcgId::NONE;
    for page in 0..pages {
        if let Some(meta) = lookup(first_page + page) {
            let id = MemcgId::from_raw(meta.take_memcg());
            if !id.is_none() {
                owner = id;
            }
        }
    }
    if let Some(node) = get(owner) {
        uncharge_bytes_class(&node, bytes as u64, class);
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
