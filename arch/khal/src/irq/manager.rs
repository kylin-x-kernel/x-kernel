// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ manager and OS-visible handler dispatch state.

use alloc::{collections::BTreeMap, sync::Arc};
use core::marker::PhantomData;
#[cfg(unittest)]
use core::sync::atomic::{AtomicUsize, Ordering};

#[cfg(feature = "ipi")]
pub use kbuild_config::IPI_IRQ;
use kcpu::excp::{IRQ, NMI, register_trap_handler};
use kspin::{SpinNoIrq, SpinRaw};

#[cfg(feature = "ipi")]
pub use self::TargetCpu as IpiTarget;
use super::{
    Hwirq, IntoIrqDesc, IrqAffinity, IrqController, IrqDesc, IrqDomainId, IrqFlags, IrqHandler,
    IrqPolarity, IrqRef, IrqSource, IrqTrigger, Virq,
};

/// IRQ handler invoked on dispatch.
///
/// Each registration owns its `Arc<dyn IrqHandler>` — the Rust-native
/// counterpart of Linux's `dev_id` — with no side table or trampoline.
pub type Handler = Arc<dyn IrqHandler>;

/// Target CPU(s) for inter-processor interrupts.
pub enum TargetCpu {
    /// Target the current CPU.
    Self_,
    /// Target a specific CPU by ID.
    Specific(usize),
    /// Target all CPUs except the caller.
    AllButSelf { me: usize, total: usize },
}

#[cfg(target_arch = "x86_64")]
#[kiface::interface]
pub trait X86ApicIf {
    fn alloc_msix_vector() -> Option<u8>;
    fn free_msix_vector(vector: u8) -> bool;
    fn current_apic_id() -> u8;
}

/// Allocates the next available MSI-X CPU vector.
///
/// Returns `None` when all vectors are exhausted.
#[cfg(target_arch = "x86_64")]
pub fn alloc_msix_vector() -> Option<u8> {
    X86ApicIf::alloc_msix_vector()
}

/// Releases a previously allocated MSI-X CPU vector.
///
/// Returns `false` if the vector is outside the MSI-X range or was not
/// currently allocated.
#[cfg(target_arch = "x86_64")]
pub fn free_msix_vector(vector: u8) -> bool {
    X86ApicIf::free_msix_vector(vector)
}

/// Returns the APIC ID of the current logical CPU.
#[cfg(target_arch = "x86_64")]
pub fn current_apic_id() -> u8 {
    X86ApicIf::current_apic_id()
}

/// An IRQ claimed by the platform and not yet completed.
///
/// Completion must happen on the CPU that claimed the interrupt.
#[derive(Debug)]
pub struct DispatchedIrq {
    irq: Virq,
    completion_cookie: usize,
    completed: bool,
    _not_send: PhantomData<*mut ()>,
}

impl DispatchedIrq {
    /// Creates a dispatched IRQ with an opaque completion cookie.
    pub const fn new(irq: Virq, completion_cookie: usize) -> Self {
        Self {
            irq,
            completion_cookie,
            completed: false,
            _not_send: PhantomData,
        }
    }

    pub const fn irq(&self) -> Virq {
        self.irq
    }

    pub fn complete(mut self) {
        self.complete_inner();
    }

    fn complete_inner(&mut self) {
        if self.completed {
            return;
        }
        self.completed = true;
        platform_complete_irq(self.completion_cookie);
    }
}

impl Drop for DispatchedIrq {
    fn drop(&mut self) {
        self.complete_inner();
    }
}

/// A pending interrupt claimed by the platform, before subscriber dispatch.
///
/// The platform reports the raw claim ([`IrqRef`]); the core resolves it to a
/// logical IRQ through the domain's lock-free reverse map and only then runs
/// subscribers. Resolution returning `None` must be reported as unhandled —
/// it never silently becomes an identity mapping.
///
/// Completion (EOI / deactivate) must happen on the CPU that claimed the
/// interrupt; like [`DispatchedIrq`], completion is idempotent and also runs
/// from `Drop` so early-return paths cannot leak an un-EOI'd interrupt.
#[derive(Debug)]
pub struct PendingIrq {
    source: IrqRef,
    completion_cookie: usize,
    completed: bool,
    _not_send: PhantomData<*mut ()>,
}

impl PendingIrq {
    /// Creates a pending IRQ claim with an opaque completion cookie.
    pub const fn new(source: IrqRef, completion_cookie: usize) -> Self {
        Self {
            source,
            completion_cookie,
            completed: false,
            _not_send: PhantomData,
        }
    }

    /// The raw claim source.
    pub const fn source(&self) -> IrqRef {
        self.source
    }

    /// Lock-free resolution to the OS-visible logical IRQ.
    ///
    /// `None` means the domain has no mapping for this hardware IRQ and no
    /// identity policy; callers must report the interrupt as unhandled and
    /// still complete the claim. Safe to call from hardirq / irqson NMI
    /// context.
    pub fn resolve(&self) -> Option<Virq> {
        match self.source {
            IrqRef::Virq(virq) => Some(virq),
            IrqRef::Domain(domain_id, hwirq) => super::domain::resolve(domain_id, hwirq),
        }
    }

    /// Completes the claim; idempotent, also run by `Drop`.
    pub fn complete(mut self) {
        self.complete_inner();
    }

    fn complete_inner(&mut self) {
        if self.completed {
            return;
        }
        self.completed = true;
        platform_complete_irq(self.completion_cookie);
    }
}

impl Drop for PendingIrq {
    fn drop(&mut self) {
        self.complete_inner();
    }
}

#[kiface::interface]
pub trait IntrManagerIf {
    fn configure(desc: IrqDesc);
    fn enable(id: usize, on: bool);
    /// Claims a pending interrupt without resolving it.
    ///
    /// The platform reports the raw [`IrqRef`]; the core resolves it through
    /// the domain's lock-free reverse map before subscriber dispatch, so a
    /// mapping lookup can never fail spuriously or fall back to the raw
    /// hardware number.
    fn dispatch_irq(id: usize) -> Option<PendingIrq>;
    /// Claims a pending NMI, returning the raw hwirq and completion cookie.
    ///
    /// Unlike [`dispatch_irq`], this bypasses virq translation and never
    /// opens the NMI window — it is only called from the NMI entry path
    /// where normal IRQs are already masked.
    fn dispatch_nmi(id: usize) -> Option<DispatchedIrq>;
    /// Completes a claimed interrupt with its opaque completion cookie.
    fn complete_irq(completion_cookie: usize);
    /// Sends an IPI to another CPU.
    ///
    /// Implementations must ensure that all Normal-memory writes performed by
    /// the caller before `notify_cpu()` become visible to the target CPU before
    /// that CPU can observe and handle the delivered IPI. TLB shootdown relies
    /// on this publish-before-notify ordering to make the request state visible
    /// before the target acknowledges it.
    fn notify_cpu(id: usize, target: TargetCpu);
    fn set_prio(id: usize, prio: u8);
}

#[inline]
fn platform_configure(desc: IrqDesc) {
    IntrManagerIf::configure(desc)
}

#[inline]
fn platform_enable(id: usize, on: bool) {
    IntrManagerIf::enable(id, on)
}

#[inline]
fn needs_platform_binding(desc: IrqDesc) -> bool {
    desc.domain.is_some()
        || desc.hwirq < DYNAMIC_VIRQ_BASE
        || !matches!(desc.trigger, IrqTrigger::Unknown(_))
        || desc.polarity != IrqPolarity::Unknown
        || desc.source != IrqSource::Unknown
        || desc.controller != IrqController::Unknown
        || desc.affinity != IrqAffinity::Any
        || !desc.flags.is_empty()
}

#[inline]
fn configure_and_enable_platform_irq(desc: IrqDesc, on: bool) {
    if needs_platform_binding(desc) {
        platform_configure(desc);
        platform_enable(desc.hwirq, on);
    }
}

#[inline]
fn disable_platform_irq(desc: IrqDesc) {
    if needs_platform_binding(desc) {
        platform_enable(desc.hwirq, false);
    }
}

#[inline]
fn platform_dispatch_irq(id: usize) -> Option<PendingIrq> {
    IntrManagerIf::dispatch_irq(id)
}

#[inline]
fn platform_dispatch_nmi(id: usize) -> Option<DispatchedIrq> {
    IntrManagerIf::dispatch_nmi(id)
}

/// Test-only EOI observability, keyed on the spurious GIC INTID (0x3FF).
///
/// Real GIC dispatch filters the spurious ID before claiming, so no live
/// interrupt ever completes with this cookie; PLIC/IO-APIC sources are far
/// below it as well. Gating the counter on it keeps the completion-invariant
/// tests immune to timer/IPI EOIs firing concurrently during the unittest
/// run. The platform `complete_irq` ignores the write for this cookie.
#[cfg(unittest)]
const TEST_COMPLETION_COOKIE: usize = 0x3FF;

#[cfg(unittest)]
static TEST_COMPLETE_IRQ_CALLS: AtomicUsize = AtomicUsize::new(0);

#[inline]
fn platform_complete_irq(completion_cookie: usize) {
    #[cfg(unittest)]
    if completion_cookie == TEST_COMPLETION_COOKIE {
        TEST_COMPLETE_IRQ_CALLS.fetch_add(1, Ordering::Relaxed);
    }
    IntrManagerIf::complete_irq(completion_cookie)
}

#[inline]
pub fn notify_cpu(id: usize, target: TargetCpu) {
    IntrManagerIf::notify_cpu(id, target)
}

#[inline]
pub fn set_prio(id: usize, prio: u8) {
    IntrManagerIf::set_prio(id, prio)
}

static IRQ_CTL: SpinNoIrq<IrqState> = SpinNoIrq::new(IrqState::new());
pub const DYNAMIC_VIRQ_BASE: Virq = 4096;

/// NMI handler table, keyed by hwirq.
///
/// # Locking invariant
///
/// - **WRITES**: boot‑time registration via [`register_nmi`] /
///   [`unregister_nmi`], or — rarely — from an NMI handler itself (see
///   [`dispatch_nmi_handler`]).  The normal IRQ path and process context never write
///   this table.
/// - **READS**: NMI context only, via [`dispatch_nmi_handler`].  The lock is never
///   acquired from a normal IRQ handler, so a pseudo‑NMI that preempts a
///   normal IRQ never contends on this lock.
///
/// [`SpinRaw`] (no IRQ / preempt guards) is therefore safe: boot‑time writers
/// run before any NMI can be delivered, and a pseudo‑NMI cannot preempt
/// another pseudo‑NMI on the same CPU, so a writer and a reader never run
/// concurrently on the same CPU.
static NMI_TABLE: SpinRaw<BTreeMap<Hwirq, Handler>> = SpinRaw::new(BTreeMap::new());

type WakeHandler = fn(usize);

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
struct MappingKey {
    domain: IrqDomainId,
    hwirq: Hwirq,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WakeupMode {
    Persistent,
    OneShot,
}

#[derive(Clone, Copy)]
struct WakeSubscription {
    mode: WakeupMode,
    armed: bool,
    handler: WakeHandler,
}

#[derive(Clone)]
struct IrqStateDesc {
    desc: IrqDesc,
    handler: Option<Handler>,
    wake_subscription: Option<WakeSubscription>,
}

impl IrqStateDesc {
    const fn new(desc: IrqDesc) -> Self {
        Self {
            desc,
            handler: None,
            wake_subscription: None,
        }
    }

    fn remember(&mut self, desc: IrqDesc) {
        self.desc = self.desc.merge(desc);
    }

    fn is_unused(&self) -> bool {
        self.handler.is_none() && self.wake_subscription.is_none()
    }
}

struct IrqState {
    descs: BTreeMap<Virq, IrqStateDesc>,
    mappings: BTreeMap<MappingKey, Virq>,
    /// Set when `resolve_desc` inserted a new domain mapping; cleared by
    /// [`IrqState::take_mappings_dirty`] after the snapshot is republished.
    mappings_dirty: bool,
    next_virq: Virq,
}

impl IrqState {
    const fn new() -> Self {
        Self {
            descs: BTreeMap::new(),
            mappings: BTreeMap::new(),
            mappings_dirty: false,
            next_virq: DYNAMIC_VIRQ_BASE,
        }
    }

    fn alloc_virq(&mut self) -> Virq {
        loop {
            let virq = self.next_virq;
            self.next_virq += 1;
            if !self.descs.contains_key(&virq) {
                return virq;
            }
        }
    }

    fn resolve_desc(&mut self, mut desc: IrqDesc) -> IrqDesc {
        let virq = if let Some(virq) = desc.logical_irq() {
            if desc.domain.is_none()
                && desc.hwirq == virq
                && matches!(desc.trigger, IrqTrigger::Unknown(_))
                && desc.polarity == IrqPolarity::Unknown
                && desc.source == IrqSource::Unknown
                && desc.controller == IrqController::Unknown
                && desc.affinity == IrqAffinity::Any
                && desc.flags.is_empty()
                && let Some(existing) = self.stored_desc(virq)
            {
                return existing;
            }
            if let Some(domain) = desc.domain {
                use alloc::collections::btree_map::Entry;
                if let Entry::Vacant(entry) = self.mappings.entry(MappingKey {
                    domain,
                    hwirq: desc.hwirq,
                }) {
                    entry.insert(virq);
                    self.mappings_dirty = true;
                }
            }
            virq
        } else if let Some(domain) = desc.domain {
            let key = MappingKey {
                domain,
                hwirq: desc.hwirq,
            };
            if let Some(&virq) = self.mappings.get(&key) {
                virq
            } else {
                let virq = self.alloc_virq();
                self.mappings.insert(key, virq);
                self.mappings_dirty = true;
                virq
            }
        } else {
            desc.hwirq
        };
        desc = desc.with_virq(virq);
        self.descs
            .entry(virq)
            .and_modify(|state| state.remember(desc))
            .or_insert_with(|| IrqStateDesc::new(desc));
        desc
    }

    fn lookup_virq(&self, desc: IrqDesc) -> Option<Virq> {
        desc.logical_irq().or_else(|| {
            desc.domain.and_then(|domain| {
                self.mappings
                    .get(&MappingKey {
                        domain,
                        hwirq: desc.hwirq,
                    })
                    .copied()
            })
        })
    }

    fn stored_desc(&self, virq: Virq) -> Option<IrqDesc> {
        self.descs.get(&virq).map(|state| state.desc)
    }

    fn take_mappings_dirty(&mut self) -> bool {
        let dirty = self.mappings_dirty;
        self.mappings_dirty = false;
        dirty
    }

    fn remove_if_unused(&mut self, virq: Virq) {
        if self.descs.get(&virq).is_some_and(IrqStateDesc::is_unused) {
            self.descs.remove(&virq);
        }
    }
}

/// Resolves a descriptor and, when it introduced a new domain mapping,
/// republishes that domain's reverse-map snapshot under the control lock.
///
/// Called from control-plane paths (`map`, `register`, `enable`, NMI
/// registration, wakeup subscription) and — through `enable()` — from the
/// dispatch path's one-shot wakeup disable in [`dispatch_subscribers`]. On
/// that data-path call the line was registered earlier, so `resolve_desc`
/// only reuses the existing mapping: no new mapping is inserted, no snapshot
/// is rebuilt or published, and no allocation happens. Any future data-path
/// caller must preserve that "mapping already exists, never publish" shape.
fn resolve_and_publish(state: &mut IrqState, desc: IrqDesc) -> IrqDesc {
    let domain_id = desc.domain;
    let desc = state.resolve_desc(desc);
    if let Some(domain_id) = domain_id
        && state.take_mappings_dirty()
    {
        let published = super::domain::publish_snapshot(domain_id, mappings_of(state, domain_id));
        if !published {
            // The mapping was inserted and a virq allocated, but the data path
            // can never resolve it: `domain_id` is absent from the static
            // domain registry, so every dispatch of this line reports
            // `Unhandled`. Fail loudly instead of silently "succeeding".
            warn!(
                "resolve_and_publish: unregistered irq domain {domain_id:?}; the mapping will \
                 never resolve on the data path"
            );
        }
    }
    desc
}

/// Yields the build-table mappings of one domain.
fn mappings_of(
    state: &IrqState,
    domain_id: IrqDomainId,
) -> impl Iterator<Item = (Hwirq, Virq)> + '_ {
    state
        .mappings
        .iter()
        .filter(move |(key, _)| key.domain == domain_id)
        .map(|(key, &virq)| (key.hwirq, virq))
}

/// Dispatches a claimed interrupt to its subscribers.
///
/// Resolution and the handler-table lookup run in one `IRQ_CTL` critical
/// section, so a concurrent `unregister` cannot interleave between them: the
/// dispatch observes either the full pre-unregister state (handler present) or
/// the post-unregister state (line unregistered, reported unhandled), never a
/// resolved mapping whose descriptor has already been removed. The handler
/// itself runs outside the lock.
fn dispatch_subscribers(pending: &PendingIrq) {
    let (virq, desc, regular_handler, wake_subscription) = {
        let mut state = IRQ_CTL.lock();
        let Some(virq) = pending.resolve() else {
            warn!("Unhandled IRQ {:?}", pending.source());
            return;
        };
        let Some(entry) = state.descs.get_mut(&virq) else {
            warn!("Unhandled IRQ {virq}");
            return;
        };
        let desc = entry.desc;
        let regular_handler = entry.handler.clone();
        let wake_subscription = match entry.wake_subscription {
            Some(subscription) if subscription.mode == WakeupMode::Persistent => Some(subscription),
            Some(subscription) if subscription.armed => {
                entry.wake_subscription = None;
                Some(subscription)
            }
            Some(_) => {
                entry.wake_subscription = None;
                None
            }
            None => None,
        };
        if wake_subscription.is_none() {
            state.remove_if_unused(virq);
        }
        (virq, desc, regular_handler, wake_subscription)
    };
    let has_regular_handler = regular_handler.is_some();

    if let Some(handler) = regular_handler {
        let _ = handler.handle();
    }

    if let Some(wake_subscription) = wake_subscription {
        if !has_regular_handler && wake_subscription.mode == WakeupMode::OneShot {
            enable(desc, false);
        }
        (wake_subscription.handler)(virq);
    } else if !has_regular_handler {
        warn!("Unhandled IRQ {virq}");
    }
}

/// Maps a hardware IRQ resource into the OS-visible logical IRQ namespace.
pub fn map(desc: impl IntoIrqDesc) -> Virq {
    let desc = desc.into_irq_desc();
    let mut state = IRQ_CTL.lock();
    resolve_and_publish(&mut state, desc).logical_irq().unwrap()
}

/// Configure and enable or disable an IRQ line.
///
/// New code should pass a full [`IrqDesc`] so trigger and polarity metadata can
/// be applied before the IRQ is enabled. Passing a plain `usize` keeps backward
/// compatibility but carries no controller metadata.
#[inline]
pub fn enable(desc: impl IntoIrqDesc, on: bool) {
    let desc = {
        let mut state = IRQ_CTL.lock();
        resolve_and_publish(&mut state, desc.into_irq_desc())
    };
    configure_and_enable_platform_irq(desc, on);
}

/// Return the descriptor currently remembered for an IRQ line, if any.
pub fn descriptor(virq: Virq) -> Option<IrqDesc> {
    IRQ_CTL.lock().stored_desc(virq)
}

/// Register the regular OS IRQ handler for an IRQ line.
///
/// Each registration carries its own `Arc<dyn IrqHandler>` — the Rust-native
/// counterpart of Linux's `dev_id` — with no side table or trampoline.
///
/// This is different from wakeup subscription: the registered handler is invoked
/// directly on dispatch, while wakeup subscribers only participate in the wakeup
/// notification path.
pub fn register(desc: impl IntoIrqDesc, handler: Handler) -> bool {
    let mut state = IRQ_CTL.lock();
    let desc = resolve_and_publish(&mut state, desc.into_irq_desc());
    let virq = desc.logical_irq().unwrap();
    let entry = state
        .descs
        .get_mut(&virq)
        .expect("descriptor state must exist after resolve_desc");
    if entry.handler.is_some() {
        warn!("register handler for IRQ {virq} failed");
        return false;
    }
    entry.handler = Some(handler);
    let desc = entry.desc;
    drop(state);
    configure_and_enable_platform_irq(desc, true);
    true
}

/// Remove the regular OS IRQ handler for an IRQ line.
pub fn unregister(desc: impl IntoIrqDesc) -> Option<Handler> {
    let desc = desc.into_irq_desc();
    let mut state = IRQ_CTL.lock();
    let virq = state.lookup_virq(desc)?;
    let (handler, removed_wakeup) = state
        .descs
        .get_mut(&virq)
        .map(|entry| {
            (
                entry.handler.take(),
                entry.wake_subscription.take().is_some(),
            )
        })
        .unwrap_or((None, false));
    if handler.is_some() {
        let disable = state.descs.get(&virq).is_some_and(IrqStateDesc::is_unused);
        let desc = state.descs.get(&virq).map(|entry| entry.desc);
        state.remove_if_unused(virq);
        drop(state);
        if disable && let Some(desc) = desc {
            disable_platform_irq(desc);
        }
        return handler;
    }
    if removed_wakeup {
        warn!("removed stale wakeup subscription for IRQ {virq} without regular handler");
        let disable = state.descs.get(&virq).is_some_and(IrqStateDesc::is_unused);
        let desc = state.descs.get(&virq).map(|entry| entry.desc);
        state.remove_if_unused(virq);
        drop(state);
        if disable && let Some(desc) = desc {
            disable_platform_irq(desc);
        }
    }
    handler
}

/// Register an NMI handler for a hardware interrupt.
///
/// The interrupt is configured as a pseudo‑NMI with the highest GIC priority
/// and routed through the lock‑free [`dispatch_nmi_handler`] path.  Unlike [`register`],
/// this function **never** acquires `IRQ_CTL.lock()` during dispatch — the
/// handler is stored in a separate [`NMI_TABLE`] keyed by hwirq.
///
/// # Safety constraints
///
/// - NMI handlers must be **per‑CPU** (enforced by tagging the descriptor with
///   `IrqFlags::PER_CPU`).
/// - NMI handlers **cannot be shared** — duplicate registration on the same
///   hwirq is rejected.
/// - Refuses to overwrite a regular handler already registered on the same
///   line (mirroring [`register`]), and rejects duplicates before touching
///   any internal state.
/// - Normally called **at boot time**, before `enable_local_irq()`, so that
///   no NMI can fire before registration is complete.  It may also be called
///   from an NMI handler itself — a pseudo‑NMI cannot preempt another
///   pseudo‑NMI on the same CPU, so the registration cannot race a reader.
pub fn register_nmi(desc: impl IntoIrqDesc, handler: Handler) -> bool {
    let desc = desc.into_irq_desc();
    let hwirq = desc.hwirq;

    // Reject duplicate NMI registrations before touching any state, so a
    // failed registration cannot leave IRQ_CTL or NMI_TABLE inconsistent.
    if NMI_TABLE.lock().contains_key(&hwirq) {
        warn!("register_nmi: handler already exists for hwirq {hwirq}");
        return false;
    }

    // Resolve descriptor in IRQ_CTL (metadata tracking + fallback handler
    // for when nmi-pmu is not enabled and dispatch goes through the normal path).
    let mut state = IRQ_CTL.lock();
    // Refuse to overwrite an existing regular handler on this line, mirroring
    // register()'s entry.handler.is_some() check.
    if let Some(virq) = state.lookup_virq(desc)
        && state
            .descs
            .get(&virq)
            .is_some_and(|entry| entry.handler.is_some())
    {
        warn!("register_nmi: handler already registered for irq {virq}");
        return false;
    }
    let desc = resolve_and_publish(&mut state, desc.with_flags(IrqFlags::PER_CPU));
    let virq = desc.logical_irq().unwrap();
    let entry = state
        .descs
        .get_mut(&virq)
        .expect("descriptor state must exist after resolve_desc");
    // Tag the remembered descriptor so descriptor() queries see the NMI flag.
    entry.desc = desc;
    // Store a fallback handler for the non‑NMI dispatch path.
    entry.handler = Some(handler.clone());
    drop(state);

    // Store handler in NMI table (keyed by hwirq).
    NMI_TABLE.lock().insert(hwirq, handler);

    // Pass the resolved descriptor so platform binding/configuration also
    // applies when the line only carries a dynamically allocated virq.
    configure_and_enable_platform_irq(desc, true);
    true
}

/// Remove a previously registered NMI handler.
///
/// Besides removing the [`NMI_TABLE`] entry, this clears the fallback handler
/// and `IrqFlags::PER_CPU` tag that [`register_nmi`] stored in `IRQ_CTL`,
/// so a re‑enabled or re‑triggered IRQ no longer dispatches the removed
/// handler through the normal path.  The platform line is disabled when it is
/// no longer used, using the full stored descriptor.
pub fn unregister_nmi(desc: impl IntoIrqDesc) -> bool {
    let desc = desc.into_irq_desc();
    let hwirq = desc.hwirq;
    let removed = {
        let mut table = NMI_TABLE.lock();
        table.remove(&hwirq).is_some()
    };
    if !removed {
        return false;
    }

    // Also clear the IRQ_CTL fallback handler and PER_CPU tag installed by
    // register_nmi, so a re-enabled or re-triggered IRQ no longer dispatches
    // the removed handler through the normal path.
    let mut state = IRQ_CTL.lock();
    let Some(virq) = state.lookup_virq(desc) else {
        return true;
    };
    if let Some(entry) = state.descs.get_mut(&virq)
        && entry.handler.is_some()
    {
        entry.handler = None;
        entry.desc = IrqDesc {
            flags: entry.desc.flags - IrqFlags::PER_CPU,
            ..entry.desc
        };
    }
    let disable = state.descs.get(&virq).is_some_and(IrqStateDesc::is_unused);
    let stored_desc = state.descs.get(&virq).map(|entry| entry.desc);
    state.remove_if_unused(virq);
    drop(state);
    if disable && let Some(stored_desc) = stored_desc {
        // Carry the full stored descriptor so the disable is not silently
        // skipped for lines whose hwirq falls above DYNAMIC_VIRQ_BASE.
        disable_platform_irq(stored_desc);
    }
    true
}

/// Dispatch a registered NMI handler without touching [`IRQ_CTL`].
///
/// The handler is cloned out of [`NMI_TABLE`] before the lock is released,
/// so the handler itself may safely (but rarely) call `register_nmi` /
/// `unregister_nmi` without self‑deadlock.
fn dispatch_nmi_handler(hwirq: Hwirq) {
    let handler = {
        let table = NMI_TABLE.lock();
        table.get(&hwirq).cloned()
    };
    if let Some(handler) = handler {
        let _ = handler.handle();
    } else {
        warn!("Unhandled NMI for hwirq {hwirq}");
    }
}

// Wakeup subscriptions do not install a regular IRQ handler. They keep the IRQ
// bound so the IRQ core can observe the event and run the wakeup callback path.
fn subscribe_wakeup_mode(desc: impl IntoIrqDesc, mode: WakeupMode, handler: WakeHandler) -> bool {
    let mut state = IRQ_CTL.lock();
    let desc = resolve_and_publish(&mut state, desc.into_irq_desc());
    let virq = desc.logical_irq().unwrap();
    let entry = state
        .descs
        .get_mut(&virq)
        .expect("descriptor state must exist after resolve_desc");
    if entry.handler.is_none() {
        warn!("subscribe wakeup for IRQ {virq} without regular handler");
        return false;
    }
    entry.wake_subscription = Some(WakeSubscription {
        mode,
        armed: true,
        handler,
    });
    let desc = entry.desc;
    drop(state);
    configure_and_enable_platform_irq(desc, true);
    true
}

pub fn subscribe_wakeup(desc: impl IntoIrqDesc, handler: WakeHandler) -> bool {
    subscribe_wakeup_mode(desc, WakeupMode::Persistent, handler)
}

pub fn subscribe_wakeup_once(desc: impl IntoIrqDesc, handler: WakeHandler) -> bool {
    subscribe_wakeup_mode(desc, WakeupMode::OneShot, handler)
}

pub fn unsubscribe_wakeup(desc: impl IntoIrqDesc) -> bool {
    let desc = desc.into_irq_desc();
    let mut state = IRQ_CTL.lock();
    let Some(virq) = state.lookup_virq(desc) else {
        return false;
    };
    let removed = state
        .descs
        .get_mut(&virq)
        .and_then(|entry| entry.wake_subscription.take())
        .is_some();
    if removed {
        let disable = state.descs.get(&virq).is_some_and(IrqStateDesc::is_unused);
        let desc = state.descs.get(&virq).map(|entry| entry.desc);
        state.remove_if_unused(virq);
        drop(state);
        if disable && let Some(desc) = desc {
            disable_platform_irq(desc);
        }
        return true;
    }
    false
}

/// Pseudo‑NMI handler.
///
/// Invoked from the exception entry layer when `dispatch_exception` detects
/// that normal IRQs were masked (PMR ≤ NMI_ONLY) at the moment the interrupt
/// fired — a pseudo‑NMI preempting a critical section.  The handler uses the
/// lock‑free [`dispatch_nmi_handler`] path which only touches [`NMI_TABLE`], never
/// [`IRQ_CTL`].
#[register_trap_handler(NMI)]
pub fn nmi_handler(vector: usize) -> bool {
    let guard = kspin::NoPreempt::new();

    if let Some(dispatched_irq) = platform_dispatch_nmi(vector) {
        dispatch_nmi_handler(dispatched_irq.irq());
        dispatched_irq.complete();
    }

    let _ = guard;
    true
}

/// Normal IRQ handler.
///
/// Invoked when the interrupted context had normal IRQs enabled (PMR >
/// NMI_ONLY). The claim is resolved inside [`dispatch_subscribers`]'s single
/// `IRQ_CTL` critical section together with the handler-table lookup; the
/// domain resolution itself remains lock-free.
///
/// # Warn
///
/// Make sure called in an interrupt context or hypervisor VM exit handler.
#[register_trap_handler(IRQ)]
pub fn irq_handler(vector: usize) -> bool {
    let guard = kspin::NoPreempt::new();

    if let Some(pending) = platform_dispatch_irq(vector) {
        dispatch_subscribers(&pending);
        pending.complete();
    }

    let _ = guard; // rescheduling may occur when preemption is re-enabled.
    true
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_irq {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicUsize, Ordering};

    use device_res::IrqEvent;
    use unittest::def_test;

    use super::{
        IRQ_CTL, IrqDesc, IrqRef, IrqStateDesc, PendingIrq, WakeSubscription, WakeupMode,
        dispatch_subscribers, irq_handler, map, unregister,
    };
    use crate::irq::{
        DYNAMIC_VIRQ_BASE, GIC_ROOT_DOMAIN, IO_APIC_DOMAIN, IrqTrigger, gic_irq_desc,
        io_apic_irq_desc,
    };

    static REGULAR_CALLS: AtomicUsize = AtomicUsize::new(0);
    static WAKE_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn test_handler() -> IrqEvent {
        REGULAR_CALLS.fetch_add(1, Ordering::Relaxed);
        IrqEvent::HANDLED
    }

    fn test_wake_handler(_irq: usize) {
        WAKE_CALLS.fetch_add(1, Ordering::Relaxed);
    }

    // Every test in this module mutates the global domain registry / IRQ_CTL
    // state (maps persist across tests and snapshots accumulate), so they must
    // run sequentially. `serial` marks them so a future parallel test runner
    // will not interleave them.
    #[def_test(serial)]
    fn test_irq_handler_returns_true() {
        #[cfg(target_arch = "riscv64")]
        const IRQ_NUM: usize = (1usize << (usize::BITS - 1)) + 1;

        #[cfg(target_arch = "x86_64")]
        const IRQ_NUM: usize = 0x10;

        #[cfg(target_arch = "aarch64")]
        const IRQ_NUM: usize = 0;

        #[cfg(target_arch = "loongarch64")]
        const IRQ_NUM: usize = 0;

        assert!(irq_handler(IRQ_NUM));
    }

    #[def_test(serial)]
    fn test_domain_resolve_mapped_and_unmapped() {
        let virq = map(gic_irq_desc(33, IrqTrigger::LevelHigh));
        assert!(virq >= DYNAMIC_VIRQ_BASE);

        let mapped = PendingIrq::new(IrqRef::Domain(GIC_ROOT_DOMAIN, 33), 0);
        assert_eq!(mapped.resolve(), Some(virq));
        core::mem::forget(mapped);

        // GIC domain has an explicit identity policy for unmapped lines
        // (arch timer / IPI are registered as plain numbers).
        let identity = PendingIrq::new(IrqRef::Domain(GIC_ROOT_DOMAIN, 999), 0);
        assert_eq!(identity.resolve(), Some(999));
        core::mem::forget(identity);

        // IO-APIC is strict: unmapped lines are explicit misses.
        let unmapped = PendingIrq::new(IrqRef::Domain(IO_APIC_DOMAIN, 999), 0);
        assert_eq!(unmapped.resolve(), None);
        core::mem::forget(unmapped);

        let direct = PendingIrq::new(IrqRef::Virq(7), 0);
        assert_eq!(direct.resolve(), Some(7));
        core::mem::forget(direct);
    }

    /// The regression invariant: resolution must not depend on the control
    /// lock. If the data path ever went back to a blocking `IRQ_CTL` lookup,
    /// this test would deadlock while holding the lock.
    #[def_test(serial)]
    fn test_resolve_ignores_control_lock() {
        let virq = map(io_apic_irq_desc(4));
        let _ctl = IRQ_CTL.lock();
        let pending = PendingIrq::new(IrqRef::Domain(IO_APIC_DOMAIN, 4), 0);
        assert_eq!(pending.resolve(), Some(virq));
        core::mem::forget(pending);
    }

    #[def_test(serial)]
    fn test_domain_snapshot_append_visible() {
        let first = map(gic_irq_desc(10, IrqTrigger::LevelHigh));
        let second = map(gic_irq_desc(11, IrqTrigger::LevelHigh));

        // Duplicate map reuses the first virq.
        assert_eq!(map(gic_irq_desc(10, IrqTrigger::LevelHigh)), first);

        // A snapshot published after the second map sees both lines.
        let pending = PendingIrq::new(IrqRef::Domain(GIC_ROOT_DOMAIN, 11), 0);
        assert_eq!(pending.resolve(), Some(second));
        core::mem::forget(pending);
    }

    /// The completion invariants the design relies on: an unhandled claim
    /// (strict domain, unmapped line) is still completed by the caller,
    /// `complete()` runs the EOI exactly once (the implicit Drop afterwards
    /// is a no-op), and `Drop` backstops early-return paths that never call
    /// `complete()`.
    ///
    /// Uses the spurious-INTID cookie so concurrent timer/IPI completions
    /// during the unittest run cannot perturb the counter.
    #[def_test(serial)]
    fn test_pending_irq_completion_invariants() {
        let before = super::TEST_COMPLETE_IRQ_CALLS.load(Ordering::Relaxed);

        // Strict-domain unmapped line: dispatch reports unhandled but must
        // not complete the claim itself; completion stays the caller's job.
        let pending = PendingIrq::new(
            IrqRef::Domain(IO_APIC_DOMAIN, 999),
            super::TEST_COMPLETION_COOKIE,
        );
        dispatch_subscribers(&pending);
        assert_eq!(
            super::TEST_COMPLETE_IRQ_CALLS.load(Ordering::Relaxed),
            before
        );

        // Explicit complete runs the EOI exactly once; the `completed` flag
        // makes the implicit Drop inside `complete()` a no-op.
        pending.complete();
        assert_eq!(
            super::TEST_COMPLETE_IRQ_CALLS.load(Ordering::Relaxed),
            before + 1
        );

        // Drop alone completes the claim: an early-return path cannot leak an
        // un-EOI'd interrupt.
        let pending = PendingIrq::new(
            IrqRef::Domain(IO_APIC_DOMAIN, 999),
            super::TEST_COMPLETION_COOKIE,
        );
        drop(pending);
        assert_eq!(
            super::TEST_COMPLETE_IRQ_CALLS.load(Ordering::Relaxed),
            before + 2
        );
    }

    #[def_test(serial)]
    fn test_unregister_clears_wakeup_subscription() {
        // Fixed virq below DYNAMIC_VIRQ_BASE: dynamic allocations start at
        // 4096, so parallel tests can never collide with this slot.
        let virq = 0x101;
        {
            let mut state = IRQ_CTL.lock();
            state.descs.insert(
                virq,
                IrqStateDesc {
                    desc: IrqDesc::from_virq(virq),
                    handler: Some(Arc::new(test_handler)),
                    wake_subscription: Some(WakeSubscription {
                        mode: WakeupMode::Persistent,
                        armed: true,
                        handler: test_wake_handler,
                    }),
                },
            );
        }

        assert!(unregister(virq).is_some());
        assert!(!IRQ_CTL.lock().descs.contains_key(&virq));
    }

    #[def_test(serial)]
    fn test_oneshot_wakeup_is_removed_after_dispatch() {
        // See test_unregister_clears_wakeup_subscription: fixed virq below
        // DYNAMIC_VIRQ_BASE keeps this test collision-free.
        let virq = 0x102;
        REGULAR_CALLS.store(0, Ordering::Relaxed);
        WAKE_CALLS.store(0, Ordering::Relaxed);

        {
            let mut state = IRQ_CTL.lock();
            state.descs.insert(
                virq,
                IrqStateDesc {
                    desc: IrqDesc::from_virq(virq),
                    handler: Some(Arc::new(test_handler)),
                    wake_subscription: Some(WakeSubscription {
                        mode: WakeupMode::OneShot,
                        armed: true,
                        handler: test_wake_handler,
                    }),
                },
            );
        }

        let pending = PendingIrq::new(IrqRef::Virq(virq), 0);
        dispatch_subscribers(&pending);
        core::mem::forget(pending);

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(WAKE_CALLS.load(Ordering::Relaxed), 1);
        let state = IRQ_CTL.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must stay for regular handler");
        assert!(entry.handler.is_some());
        assert!(entry.wake_subscription.is_none());
    }
}
