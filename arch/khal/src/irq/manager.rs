// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ manager and OS-visible handler dispatch state.

use alloc::{collections::BTreeMap, sync::Arc};
use core::marker::PhantomData;

#[cfg(feature = "ipi")]
pub use kbuild_config::IPI_IRQ;
use kcpu::excp::{IRQ, NMI, register_trap_handler};
use kspin::{SpinNoIrq, SpinRaw};

#[cfg(feature = "ipi")]
pub use self::TargetCpu as IpiTarget;
use super::{
    Hwirq, IntoIrqDesc, IrqAffinity, IrqController, IrqDesc, IrqDomainId, IrqFlags, IrqHandler,
    IrqPolarity, IrqSource, IrqTrigger, Virq,
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

#[kiface::interface]
pub trait IntrManagerIf {
    fn configure(desc: IrqDesc);
    fn enable(id: usize, on: bool);
    /// Claims a pending interrupt and returns a guard that completes it on drop.
    fn dispatch_irq(id: usize) -> Option<DispatchedIrq>;
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
fn platform_dispatch_irq(id: usize) -> Option<DispatchedIrq> {
    IntrManagerIf::dispatch_irq(id)
}

#[inline]
fn platform_dispatch_nmi(id: usize) -> Option<DispatchedIrq> {
    IntrManagerIf::dispatch_nmi(id)
}

#[inline]
fn platform_complete_irq(completion_cookie: usize) {
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

static IRQ_STATE: SpinNoIrq<IrqState> = SpinNoIrq::new(IrqState::new());
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
    next_virq: Virq,
}

impl IrqState {
    const fn new() -> Self {
        Self {
            descs: BTreeMap::new(),
            mappings: BTreeMap::new(),
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
                self.mappings
                    .entry(MappingKey {
                        domain,
                        hwirq: desc.hwirq,
                    })
                    .or_insert(virq);
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

    fn translated_hwirq(&self, domain: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
        self.mappings.get(&MappingKey { domain, hwirq }).copied()
    }

    fn remove_if_unused(&mut self, virq: Virq) {
        if self.descs.get(&virq).is_some_and(IrqStateDesc::is_unused) {
            self.descs.remove(&virq);
        }
    }
}

fn dispatch_subscribers(virq: Virq) {
    let (desc, regular_handler, wake_subscription) = {
        let mut state = IRQ_STATE.lock();
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
        (desc, regular_handler, wake_subscription)
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
    let mut state = IRQ_STATE.lock();
    state.resolve_desc(desc).logical_irq().unwrap()
}

/// Returns the mapped logical IRQ number for a domain-local hardware IRQ.
///
/// NMI-safe: if `IRQ_STATE` is already held (NMI re-entered a normal IRQ
/// handler on the same CPU), returns `None` instead of blocking.  Callers
/// such as [`resolve_hwirq`] then fall back to the raw hwirq themselves;
/// returning the untranslated hwirq here would misreport a dynamically
/// allocated virq (≥ [`DYNAMIC_VIRQ_BASE`]) as the hwirq.
pub fn translate_hwirq(domain: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
    let state = IRQ_STATE.try_lock()?;
    state.translated_hwirq(domain, hwirq)
}

/// Resolves a hardware IRQ to a logical IRQ, falling back to the raw hardware
/// number when no explicit mapping exists yet.
pub fn resolve_hwirq(domain: IrqDomainId, hwirq: Hwirq) -> Virq {
    translate_hwirq(domain, hwirq).unwrap_or(hwirq)
}

/// Configure and enable or disable an IRQ line.
///
/// New code should pass a full [`IrqDesc`] so trigger and polarity metadata can
/// be applied before the IRQ is enabled. Passing a plain `usize` keeps backward
/// compatibility but carries no controller metadata.
#[inline]
pub fn enable(desc: impl IntoIrqDesc, on: bool) {
    let desc = {
        let mut state = IRQ_STATE.lock();
        state.resolve_desc(desc.into_irq_desc())
    };
    configure_and_enable_platform_irq(desc, on);
}

/// Return the descriptor currently remembered for an IRQ line, if any.
pub fn descriptor(virq: Virq) -> Option<IrqDesc> {
    IRQ_STATE.lock().stored_desc(virq)
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
    let mut state = IRQ_STATE.lock();
    let desc = state.resolve_desc(desc.into_irq_desc());
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
    let mut state = IRQ_STATE.lock();
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
/// this function **never** acquires `IRQ_STATE.lock()` during dispatch — the
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
    // failed registration cannot leave IRQ_STATE or NMI_TABLE inconsistent.
    if NMI_TABLE.lock().contains_key(&hwirq) {
        warn!("register_nmi: handler already exists for hwirq {hwirq}");
        return false;
    }

    // Resolve descriptor in IRQ_STATE (metadata tracking + fallback handler
    // for when nmi-pmu is not enabled and dispatch goes through the normal path).
    let mut state = IRQ_STATE.lock();
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
    let desc = state.resolve_desc(desc.with_flags(IrqFlags::PER_CPU));
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
/// and `IrqFlags::PER_CPU` tag that [`register_nmi`] stored in `IRQ_STATE`,
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

    // Also clear the IRQ_STATE fallback handler and PER_CPU tag installed by
    // register_nmi, so a re-enabled or re-triggered IRQ no longer dispatches
    // the removed handler through the normal path.
    let mut state = IRQ_STATE.lock();
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

/// Dispatch a registered NMI handler without touching [`IRQ_STATE`].
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
    let mut state = IRQ_STATE.lock();
    let desc = state.resolve_desc(desc.into_irq_desc());
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
    let mut state = IRQ_STATE.lock();
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
/// [`IRQ_STATE`].
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
/// NMI_ONLY).  No critical‑section locks are held, so it is safe to acquire
/// [`IRQ_STATE`].
///
/// # Warn
///
/// Make sure called in an interrupt context or hypervisor VM exit handler.
#[register_trap_handler(IRQ)]
pub fn irq_handler(vector: usize) -> bool {
    let guard = kspin::NoPreempt::new();

    if let Some(dispatched_irq) = platform_dispatch_irq(vector) {
        dispatch_subscribers(dispatched_irq.irq());
        dispatched_irq.complete();
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
        IRQ_STATE, IrqDesc, IrqStateDesc, WakeSubscription, WakeupMode, dispatch_subscribers,
        irq_handler, unregister,
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

    #[def_test]
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

    #[def_test]
    fn test_unregister_clears_wakeup_subscription() {
        let virq = 0x2001;
        {
            let mut state = IRQ_STATE.lock();
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
        assert!(!IRQ_STATE.lock().descs.contains_key(&virq));
    }

    #[def_test]
    fn test_oneshot_wakeup_is_removed_after_dispatch() {
        let virq = 0x2002;
        REGULAR_CALLS.store(0, Ordering::Relaxed);
        WAKE_CALLS.store(0, Ordering::Relaxed);

        {
            let mut state = IRQ_STATE.lock();
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

        dispatch_subscribers(virq);

        assert_eq!(REGULAR_CALLS.load(Ordering::Relaxed), 1);
        assert_eq!(WAKE_CALLS.load(Ordering::Relaxed), 1);
        let state = IRQ_STATE.lock();
        let entry = state
            .descs
            .get(&virq)
            .expect("descriptor must stay for regular handler");
        assert!(entry.handler.is_some());
        assert!(entry.wake_subscription.is_none());
    }
}
