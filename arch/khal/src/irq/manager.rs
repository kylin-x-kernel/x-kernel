// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ manager and OS-visible handler dispatch state.

use alloc::collections::BTreeMap;

use crate_interface::{call_interface, def_interface};
#[cfg(feature = "ipi")]
pub use kbuild_config::IPI_IRQ;
use kcpu::excp::{IRQ, register_trap_handler};
use kspin::SpinNoIrq;

#[cfg(feature = "ipi")]
pub use self::TargetCpu as IpiTarget;
use super::{
    Hwirq, IntoIrqDesc, IrqAffinity, IrqControllerKind, IrqDesc, IrqDomainId, IrqPolarity,
    IrqSource, IrqTrigger, Virq,
};

/// IRQ handler type.
pub type Handler = handler_table::Handler;

/// Target CPU(s) for inter-processor interrupts.
pub enum TargetCpu {
    /// Target the current CPU.
    Self_,
    /// Target a specific CPU by ID.
    Specific(usize),
    /// Target all CPUs except the caller.
    AllButSelf { me: usize, total: usize },
}

// Platform-provided MSI-X helpers (x86_64 only).
// The implementations live in the selected x86 platform crate and are linked
// in via the exported symbol names below.
#[cfg(target_arch = "x86_64")]
unsafe extern "Rust" {
    #[link_name = "__kplat_alloc_msix_vector"]
    fn __alloc_msix_vector_impl() -> Option<u8>;
    #[link_name = "__kplat_current_apic_id"]
    fn __current_apic_id_impl() -> u8;
}

/// Allocates the next available MSI-X CPU vector.
///
/// Returns `None` when all vectors are exhausted.
#[cfg(target_arch = "x86_64")]
pub fn alloc_msix_vector() -> Option<u8> {
    unsafe { __alloc_msix_vector_impl() }
}

/// Returns the APIC ID of the current logical CPU.
#[cfg(target_arch = "x86_64")]
pub fn current_apic_id() -> u8 {
    unsafe { __current_apic_id_impl() }
}

#[def_interface]
pub trait IntrManagerIf {
    fn configure(desc: IrqDesc);
    fn enable(id: usize, on: bool);
    fn dispatch_irq(id: usize) -> Option<usize>;
    fn notify_cpu(id: usize, target: TargetCpu);
    fn set_prio(id: usize, prio: u8);
}

#[inline]
fn platform_configure(desc: IrqDesc) {
    call_interface!(IntrManagerIf::configure, desc)
}

#[inline]
fn platform_enable(id: usize, on: bool) {
    call_interface!(IntrManagerIf::enable, id, on)
}

#[inline]
fn needs_platform_binding(desc: IrqDesc) -> bool {
    desc.domain.is_some()
        || desc.hwirq < DYNAMIC_VIRQ_BASE
        || !matches!(desc.trigger, IrqTrigger::Unknown(_))
        || desc.polarity != IrqPolarity::Unknown
        || desc.source != IrqSource::Unknown
        || desc.controller != IrqControllerKind::Unknown
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
fn platform_dispatch_irq(id: usize) -> Option<usize> {
    call_interface!(IntrManagerIf::dispatch_irq, id)
}

#[inline]
pub fn notify_cpu(id: usize, target: TargetCpu) {
    call_interface!(IntrManagerIf::notify_cpu, id, target)
}

#[inline]
pub fn set_prio(id: usize, prio: u8) {
    call_interface!(IntrManagerIf::set_prio, id, prio)
}

static IRQ_STATE: SpinNoIrq<IrqState> = SpinNoIrq::new(IrqState::new());
pub const DYNAMIC_VIRQ_BASE: Virq = 4096;

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

#[derive(Clone, Copy)]
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
                && desc.controller == IrqControllerKind::Unknown
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
        let regular_handler = entry.handler;
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
        handler();
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
pub fn translate_hwirq(domain: IrqDomainId, hwirq: Hwirq) -> Option<Virq> {
    IRQ_STATE.lock().translated_hwirq(domain, hwirq)
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

/// IRQ handler.
///
/// # Warn
///
/// Make sure called in an interrupt context or hypervisor VM exit handler.
#[register_trap_handler(IRQ)]
pub fn irq_handler(vector: usize) -> bool {
    let guard = kspin::NoPreempt::new();

    if let Some(irq) = platform_dispatch_irq(vector) {
        dispatch_subscribers(irq);
    }

    let _ = guard; // rescheduling may occur when preemption is re-enabled.
    true
}

#[cfg(unittest)]
#[allow(missing_docs)]
pub mod tests_irq {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use unittest::def_test;

    use super::{
        IRQ_STATE, IrqDesc, IrqStateDesc, WakeSubscription, WakeupMode, dispatch_subscribers,
        irq_handler, unregister,
    };

    static REGULAR_CALLS: AtomicUsize = AtomicUsize::new(0);
    static WAKE_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn test_handler() {
        REGULAR_CALLS.fetch_add(1, Ordering::Relaxed);
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
                    handler: Some(test_handler),
                    wake_subscription: Some(WakeSubscription {
                        mode: WakeupMode::Persistent,
                        armed: true,
                        handler: test_wake_handler,
                    }),
                },
            );
        }

        assert!(unregister(virq).is_some());
        assert!(IRQ_STATE.lock().descs.get(&virq).is_none());
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
                    handler: Some(test_handler),
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
