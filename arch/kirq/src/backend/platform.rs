// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Platform IRQ manager interface and claimed-interrupt guard.
//!
//! `kirq` owns generic descriptor, dispatch, and completion ordering. Platform
//! irqchip backends implement this module's trait with concrete controller
//! behavior such as line configuration, mask/unmask, claim/ack, EOI or
//! deactivate, priority, and IPI delivery.

use alloc::sync::Arc;
use core::marker::PhantomData;

use crate::{
    IrqAffinity, IrqController, IrqDesc, IrqFlags, IrqHandler, IrqPolarity, IrqRef, IrqSource,
    IrqTrigger, Virq, state::DYNAMIC_VIRQ_BASE,
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

/// An IRQ claimed by the platform and not yet completed.
///
/// Completion must happen on the CPU that claimed the interrupt. The guard is
/// not `Send`, and its `Drop` path completes the interrupt if the generic IRQ
/// tail did not call [`DispatchedIrq::complete`] explicitly.
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

    /// Returns the interrupt number claimed by the backend.
    ///
    /// Normal IRQ dispatch returns an OS-visible virq. NMI dispatch returns a
    /// raw hwirq because pseudo-NMI dispatch is intentionally independent from
    /// the normal `IRQ_STATE` translation table.
    pub const fn irq(&self) -> Virq {
        self.irq
    }

    /// Completes the claimed interrupt exactly once.
    ///
    /// Generic IRQ dispatch calls this after primary handler and wake
    /// compatibility fanout. Edge-style backends may have already EOIed in
    /// dispatch and use a no-op cookie, but level-triggered backends must delay
    /// their final EOI or deactivate until this point.
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
    pub fn resolve(&self) -> Option<Virq> {
        match self.source {
            IrqRef::Virq(virq) => Some(virq),
            IrqRef::Domain(domain_id, hwirq) => crate::domain::resolve(domain_id, hwirq),
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
    /// Configures a controller-local IRQ source from a normalized descriptor.
    ///
    /// `kirq` calls this before enabling a non-MSI line. Backends should consume
    /// trigger, polarity, controller, source, affinity, and flags metadata that
    /// they understand, and ignore metadata that is not relevant to that
    /// controller. MSI descriptors are programmed through `MsiBackendIf` and do
    /// not use the normal line configure path.
    fn configure(desc: IrqDesc);

    /// Enables or disables a controller-local IRQ source.
    ///
    /// `id` is the backend hwirq or local source id, not a dynamic virq.
    /// Enabling is normally called after [`IntrManagerIf::configure`].
    /// Disabling should mask or otherwise stop delivery of that local source.
    /// MSI enablement is represented by the device-visible MSI message and does
    /// not use this hook.
    fn enable(id: usize, on: bool);

    /// Claims a pending interrupt without resolving it.
    ///
    /// The backend must perform the controller claim and any required ack, but
    /// must leave domain-local hwirq to virq translation to the returned
    /// [`PendingIrq`]. `None` means spurious or no claim and must not trigger
    /// generic handler dispatch.
    fn dispatch_irq(id: usize) -> Option<PendingIrq>;

    /// Claims a pending NMI, returning the raw hwirq and completion cookie.
    ///
    /// Unlike [`IntrManagerIf::dispatch_irq`], this bypasses virq translation
    /// and never opens the normal IRQ window. It is only called from the NMI
    /// entry path where normal IRQs are already masked.
    fn dispatch_nmi(id: usize) -> Option<DispatchedIrq>;

    /// Completes a claimed interrupt with its opaque completion cookie.
    ///
    /// `completion_cookie` must be the cookie returned by the matching dispatch
    /// call and must be completed on the same CPU that claimed it. Backends may
    /// treat cookie `0` as a no-op for interrupt classes that were completed
    /// during dispatch, but level-triggered lines must use this hook for their
    /// final EOI or deactivate after generic handler fanout.
    fn complete_irq(completion_cookie: usize);

    /// Sends an IPI to another CPU.
    ///
    /// Implementations must ensure that all Normal-memory writes performed by
    /// the caller before `notify_cpu()` become visible to the target CPU before
    /// that CPU can observe and handle the delivered IPI. TLB shootdown relies
    /// on this publish-before-notify ordering to make the request state visible
    /// before the target acknowledges it.
    fn notify_cpu(id: usize, target: TargetCpu);

    /// Sets a controller-local interrupt priority.
    ///
    /// Backends that do not expose priority levels may implement this as a
    /// no-op. The `id` is controller-local and follows the same namespace as
    /// [`IntrManagerIf::enable`].
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
    if desc.flags.contains(IrqFlags::MSI) {
        return false;
    }
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
pub(crate) fn configure_and_enable_platform_irq(desc: IrqDesc, on: bool) {
    if needs_platform_binding(desc) {
        platform_configure(desc);
        platform_enable(desc.hwirq, on);
    }
}

#[inline]
pub(crate) fn disable_platform_irq(desc: IrqDesc) {
    if needs_platform_binding(desc) {
        platform_enable(desc.hwirq, false);
    }
}

#[inline]
pub(crate) fn platform_dispatch_irq(id: usize) -> Option<PendingIrq> {
    IntrManagerIf::dispatch_irq(id)
}

#[inline]
pub(crate) fn platform_dispatch_nmi(id: usize) -> Option<DispatchedIrq> {
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
