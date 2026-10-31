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

/// A platform IRQ whose host completion obligation has not yet been consumed.
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
    /// The generic helper calls this after handler execution. The backend
    /// performs the remaining host-owned operations for this claim; operations
    /// already performed during acknowledgement must not be repeated. A
    /// backend-specific forwarding protocol may leave deactivation to a guest.
    /// Return does not by itself prove hardware inactivity or device shutdown.
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

/// A platform claim awaiting generic action dispatch and host completion.
///
/// This guard owns the remaining host completion obligation, not necessarily
/// all hardware activity. The backend may already have acknowledged the source
/// or may transfer deactivation through a separate forwarding protocol.
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

    /// Consumes the host completion obligation, also handled by `Drop`.
    ///
    /// The backend hook runs once for this guard on normal completion/drop.
    /// Its return does not guarantee hardware inactivity or stop future arrivals.
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
    /// Must obey the same bounded, non-sleeping and non-reentrant controller
    /// contract as [`IntrManagerIf::enable`]; Line state may be locked here too.
    fn configure(desc: IrqDesc);

    /// Enables or disables a controller-local IRQ source.
    ///
    /// `id` is the backend hwirq or local source id, not a dynamic virq.
    /// Enabling is normally called after [`IntrManagerIf::configure`].
    /// Disabling should mask or otherwise stop delivery of that local source.
    /// MSI enablement is represented by the device-visible MSI message and does
    /// not use this hook.
    ///
    /// Must be bounded and non-sleeping. KIRQ may hold the selected Line's
    /// IRQ-safe state lock across this call. The provider may acquire its own
    /// controller-register locks, but must not reenter KIRQ registry, control
    /// or dispatch paths while holding those locks or servicing this call.
    fn enable(id: usize, on: bool);

    /// Claims, classifies, and dispatches an interrupt taken with IRQs enabled.
    ///
    /// After the controller claim and required ack, the backend must call
    /// exactly one generic entry:
    ///
    /// - [`crate::generic_handle_irq`] for a normal IRQ;
    /// - [`crate::generic_handle_nmi`] for an NMI-class claim.
    ///
    /// The return value is the resolved normal `virq`, when one exists, and is
    /// used only as metadata by the outer deferred-execution hook. `None` may
    /// therefore mean an NMI-class claim, an unresolved normal claim, or a
    /// spurious vector; dispatch and completion have already happened before
    /// this method returns.
    fn dispatch_irq(id: usize) -> Option<Virq>;

    /// Claims and dispatches an interrupt taken through the NMI entry path.
    ///
    /// The backend must call [`crate::generic_handle_nmi`] with a
    /// [`DispatchedIrq`] containing the raw hwirq and completion cookie. Unlike
    /// [`IntrManagerIf::dispatch_irq`], this path never opens the normal IRQ
    /// window. It is only called where normal IRQs are already masked.
    fn dispatch_nmi(id: usize);

    /// Performs the remaining host-owned completion operations for a claim.
    ///
    /// `completion_cookie` must be the cookie returned by the matching dispatch
    /// call and must be completed on the same CPU that claimed it. The provider
    /// owns the cookie encoding, including any no-op sentinel. It must not repeat
    /// operations already performed before generic dispatch or omit operations
    /// still required by its source/controller protocol, even when no action ran.
    /// Trigger type alone does not determine EOI/deactivation timing.
    ///
    /// A supported forwarding protocol may delegate deactivation to another
    /// owner. Its lifecycle must be established separately; returning here is
    /// not a hardware-active query, a device shutdown or a forwarding drain.
    /// The hook must not sleep or migrate. NMI-class providers must also obey
    /// NMI-context restrictions; ordinary providers may use bounded chip locks.
    fn complete_irq(completion_cookie: usize);

    /// Sends an IPI to another CPU.
    ///
    /// Implementations must ensure that all Normal-memory writes performed by
    /// the caller before `notify_cpu()` become visible to the target CPU before
    /// that CPU can observe and handle the delivered IPI. TLB shootdown relies
    /// on this publish-before-notify ordering to make the request state visible
    /// before the target acknowledges it.
    fn notify_cpu(id: usize, target: TargetCpu);

    /// Prepares a permanent local ordinary-IRQ notification for irq_work.
    ///
    /// Called once per CPU in non-migrating normal startup context, after the
    /// allocator and local interrupt controller are ready. Preparation may
    /// allocate and acquire locks, but must not sleep. The provider installs
    /// the global handler once and configures/enables delivery on this CPU.
    /// The handler must call [`crate::irq_work::handle_irq`].
    ///
    /// Returns `None` when unsupported or preparation fails. On success the
    /// function remains valid for the kernel lifetime on this prepared CPU:
    /// it must be NMI-safe, reentrant, allocation-free, lock-free and log-free,
    /// with no lazy initialization. It orders preceding memory writes before
    /// notifying this CPU through an ordinary IRQ, never an NMI. Callers must
    /// prevent migration and publish irq_work readiness only after this returns.
    fn prepare_local_irq_work() -> Option<fn()>;

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
pub(crate) fn configure_platform_irq(desc: IrqDesc) {
    if needs_platform_binding(desc) {
        platform_configure(desc);
    }
}

#[inline]
pub(crate) fn set_platform_irq_enabled(desc: IrqDesc, on: bool) {
    if needs_platform_binding(desc) {
        platform_enable(desc.hwirq, on);
    }
}

#[inline]
pub(crate) fn platform_dispatch_irq(id: usize) -> Option<Virq> {
    IntrManagerIf::dispatch_irq(id)
}

#[inline]
pub(crate) fn platform_dispatch_nmi(id: usize) {
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
