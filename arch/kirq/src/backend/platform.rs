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

/// Source operations required by ordinary, non-oneshot fasteoi handling.
///
/// The provider retains these functions and their controller for kernel life.
/// All operations are bounded, non-sleeping and non-reentrant into KIRQ, and
/// execute under the original Line lock. They control flow suppression and
/// pending delivery, not logical disable depth, routing or configured priority.
#[derive(Debug)]
pub struct FasteoiChip {
    /// Requests delivery suppression until explicit unmask. An infallible chip
    /// may diagnose a bounded hardware timeout and return; this is not a
    /// hardware-quiescence guarantee. KIRQ rejects ineligible arriving claims
    /// independently of whether physical masking has completed.
    pub mask: fn(IrqDesc),
    /// Restores delivery using the latest configured source priority.
    pub unmask: fn(IrqDesc),
    /// Selects how eligible, non-level pending work is retriggered.
    pub resend: FasteoiResend,
}

/// Operations for edge flow. Claims are acknowledged under the Line lock
/// before handlers run; overlapping arrivals latch pending for the running owner.
/// Operations obey the same bounded, non-reentrant contract as [`FasteoiChip`].
#[derive(Debug)]
pub struct EdgeChip {
    /// Suppresses source delivery where supported. For device-owned MSI masking,
    /// this may be a no-op: generic admission still rejects disabled arrivals,
    /// but the device owner must stop the producer before teardown.
    pub mask: fn(IrqDesc),
    /// Restores flow-suppressed delivery without changing logical disable depth.
    pub unmask: fn(IrqDesc),
    /// Requests delivery to the retained source routing, under Line exclusion.
    /// True consumes pending, false retains it for later management retry.
    /// Acceptance is not delivery completion or a vector-reuse barrier.
    pub retrigger: fn(IrqDesc) -> bool,
}

/// Ordinary level flow operations, under the original Line lock. The provider
/// masks and acknowledges the leaf before actions; unmask permits hardware
/// reassertion. Hooks obey the bounded, non-reentrant FasteoiChip contract.
#[derive(Debug)]
pub struct LevelChip {
    /// Masks the source and acknowledges any leaf obligation.
    pub mask_ack: fn(IrqDesc),
    /// Reopens the source after actions if it is still eligible.
    pub unmask: fn(IrqDesc),
}

/// CPU-local guest completion delegation, independent of source enablement.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PerCpuForwardingState {
    /// Producer stopped and delivery masked; revoke delegation and clear active.
    Host,
    /// Delegate deactivation and reconstruct the destination's physical activity.
    Guest { is_active: bool },
}

/// Permanent irqchip operations for a validated CPU-local forwarding source.
///
/// Called with ordinary IRQs masked, never in NMI, after source-owner exclusion.
/// Must not sleep, allocate, reenter KIRQ, or change IRQ/preemption state.
pub struct PerCpuForwardingChip {
    /// Updates delegation and physical active state as one controller transaction.
    /// Source masking and producer shutdown are the caller's responsibility.
    ///
    /// # Safety
    /// Use the descriptor admitted by `percpu_forwarding`; retain its source
    /// ownership and registered action, exclude competing management, and keep
    /// IRQs/migration disabled outside NMI. No claim may cross the transition.
    /// Host requires stopped output and masked delivery; Guest requires masked
    /// delivery and the owner's reconstructed logical active state.
    pub set_state: unsafe fn(IrqDesc, PerCpuForwardingState),
}

/// Firmware-described hardware virtualization interfaces owned by an irqchip.
/// Physical ranges are validated by the provider; consumers must still map
/// them as device memory and validate controller-specific register capacity.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum GuestIrqInterface {
    /// GICv2 virtual interface control (GICH) and guest CPU interface (GICV).
    GicV2 {
        /// Physical base of the GICH register page.
        control_paddr: usize,
        /// Physical base of the two-page GICV CPU interface.
        cpu_paddr: usize,
    },
}

/// Pending-service policy for an ordinary fasteoi source.
#[derive(Clone, Copy, Debug)]
pub enum FasteoiResend {
    /// The provider validated level-triggered hardware, including when firmware
    /// omitted the trigger. Unmask allows reassertion; never synthesize replay.
    Level,
    /// Allows software action service by the permanent BH executor.
    Software,
    /// Requests a real hardware delivery under the original Line lock with
    /// local IRQs disabled. The callback must be bounded, non-sleeping and
    /// non-reentrant into KIRQ, and remain valid for kernel life.
    ///
    /// True means the request was accepted and consumes pending; repeated
    /// scheduling without new pending work does not request another delivery.
    /// False must mean no request was accepted: pending is retained, with no
    /// software fallback or automatic retry guarantee. Later management
    /// progress may retry. No software queue or in-flight obligation is added.
    /// Providers with reachable failures need a failure-reporting and recovery
    /// policy before production integration; retaining pending is not recovery.
    /// Explicit level sources instead clear pending and rely on hardware.
    /// Ordinary GIC SPIs use a pending-bit write: acceptance means the write
    /// was issued, not that delivery or handler execution has completed.
    HardwareOnly(fn(IrqDesc) -> bool),
}

/// Validated source flow selected before descriptor publication.
#[derive(Clone, Copy, Debug)]
pub enum IrqFlow {
    /// Ordinary fasteoi with explicit resend policy; excludes per-CPU and NMI.
    Fasteoi(&'static FasteoiChip),
    /// Edge acknowledgement before handlers, with serialized pending service.
    Edge(&'static EdgeChip),
    /// Mask/ack before actions and conditional unmask afterward; no replay IPI.
    Level(&'static LevelChip),
    /// CPU-local enable/configuration state; callbacks may run on different CPUs
    /// concurrently. The provider owns local acknowledgement/completion timing.
    PerCpu,
}

impl IrqFlow {
    pub(crate) fn unmask(self) -> Option<fn(IrqDesc)> {
        match self {
            Self::PerCpu => None,
            Self::Fasteoi(chip) => Some(chip.unmask),
            Self::Edge(chip) => Some(chip.unmask),
            Self::Level(chip) => Some(chip.unmask),
        }
    }
}

impl PartialEq for IrqFlow {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::PerCpu, Self::PerCpu) => true,
            (Self::Fasteoi(left), Self::Fasteoi(right)) => core::ptr::eq(*left, *right),
            (Self::Edge(left), Self::Edge(right)) => core::ptr::eq(*left, *right),
            (Self::Level(left), Self::Level(right)) => core::ptr::eq(*left, *right),
            _ => false,
        }
    }
}

impl Eq for IrqFlow {}

/// Target CPU(s) for inter-processor interrupts.
pub enum TargetCpu {
    /// Target the current CPU.
    Self_,
    /// Target a specific CPU by ID.
    Specific(usize),
    /// Target all CPUs except the caller.
    AllButSelf { me: usize, total: usize },
}

/// A platform claim awaiting generic action dispatch and host completion.
///
/// This guard owns the remaining host completion obligation, not necessarily
/// all hardware activity. The backend may already have acknowledged the source
/// or may transfer deactivation through a separate forwarding protocol.
/// Both normal IRQ and NMI dispatch consume this same type. It is not `Send`;
/// callers additionally exclude migration until completion on the claiming CPU.
/// Dropping it completes the claim, but does not unregister an action or stop
/// the device that produced the interrupt.
#[derive(Debug)]
pub struct PendingIrq {
    source: IrqRef,
    completion_cookie: usize,
    completed: bool,
    _not_send: PhantomData<*mut ()>,
}

impl PendingIrq {
    /// Pure software fixture with no controller completion obligation.
    #[cfg(unittest)]
    pub(crate) const fn simulated(source: IrqRef) -> Self {
        Self {
            source,
            completion_cookie: 0,
            completed: true,
            _not_send: PhantomData,
        }
    }

    #[cfg(unittest)]
    pub(crate) fn has_no_completion_for_tests(&self) -> bool {
        self.completed
    }

    /// Creates a pending IRQ claim with an opaque completion cookie.
    ///
    /// # Safety
    /// The caller uniquely owns an acknowledged controller claim (or a
    /// backend-defined no-completion sentinel). `source` and the cookie must
    /// describe that claim. Construct inside generic hardirq or effective NMI
    /// context, with RCU watching active on an initialized CPU. Retain that
    /// entry scope, controller validity and ordinary IRQ/migration exclusion
    /// on the claiming CPU until this value is consumed or dropped; do not
    /// duplicate or independently complete its obligation. Normal-flow
    /// consumption rejects effective NMI context instead of taking IRQ locks.
    pub const unsafe fn new(source: IrqRef, completion_cookie: usize) -> Self {
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
        // SAFETY: construction transfers one valid claim and requires its CPU,
        // controller and execution exclusion to remain valid through completion.
        unsafe { IntrManagerIf::complete_irq(self.completion_cookie) };
    }
}

impl Drop for PendingIrq {
    fn drop(&mut self) {
        self.complete_inner();
    }
}

/// Controller operations for one CPU's NMI configuration.
///
/// Both operations run under Line exclusion with IRQs and migration disabled,
/// outside NMI. They must be bounded, non-sleeping and non-reentrant; they must
/// not call source callbacks or reacquire KIRQ locks. The local line is already
/// logically disabled. Preparation verifies hardware disable completion before
/// promotion. Teardown verifies disable completion, rejects active delivery,
/// clears stale pending state and restores ordinary delivery attributes.
/// Neither operation enables the source. Errors retain the local preparation
/// obligation and prohibit enable until successful preparation or teardown.
pub struct PerCpuNmiChip {
    /// Installs local NMI attributes after checking the disabled source.
    pub prepare: fn(IrqDesc) -> Result<(), crate::IrqDescError>,
    /// Removes local attributes after the source owner stopped generation.
    pub teardown: fn(IrqDesc) -> Result<(), crate::IrqDescError>,
}

#[kiface::interface]
pub trait IntrManagerIf {
    /// Selects immediate affinity operations for this exact source, without
    /// allocation or register access, under Line exclusion. None means unsupported,
    /// including controllers whose deferred migration protocol is not installed.
    /// The returned operations and controller remain valid for kernel life.
    /// Selection depends on source capability, not current route health: a failed
    /// transaction must not hide the operations needed to recover that source.
    fn immediate_affinity(desc: IrqDesc) -> Option<&'static crate::IrqAffinityChip>;

    /// Returns a deferred routing capability for an ordinary source, if any.
    fn deferred_affinity(desc: IrqDesc) -> Option<&'static crate::IrqDeferredAffinityChip>;
    /// Selects CPU-local NMI control for an admitted descriptor, without writes.
    /// None means the controller cannot provide the local NMI lifecycle.
    fn percpu_nmi(desc: IrqDesc) -> Option<&'static PerCpuNmiChip>;
    /// Returns explicitly discovered guest-interface capability, not a platform
    /// address guess. None forbids guest-interface MMIO mapping.
    fn guest_interface() -> Option<GuestIrqInterface>;
    /// Returns forwarding operations for a supported private source, or None.
    /// Must validate controller identity/range without modifying hardware.
    /// Absence is not permission to emulate delegation by omitting completion.
    fn percpu_forwarding(desc: IrqDesc) -> Option<&'static PerCpuForwardingChip>;
    /// Selects supported flow operations without allocation or hardware writes.
    ///
    /// Called under registry and Line exclusion, before publishing merged
    /// metadata. Validate controller identity, source range and flow eligibility.
    /// Unknown source identities and unsupported controller combinations return
    /// UnsupportedFlow, never a silent fallback. Explicit platform-local timer
    /// and IPI identities may use a flat namespace without a controller tag.
    fn select_flow(desc: IrqDesc) -> Result<IrqFlow, crate::IrqDescError>;

    /// Configures a controller-local IRQ source from a normalized descriptor.
    ///
    /// `kirq` calls this before publishing a new action when configuration is
    /// stale, including disabled registration, or before enabling a stale
    /// non-MSI line. Configuration must not activate an initially disabled
    /// source; a successful transaction may restore prior hardware enablement
    /// after temporary masking. Logical activation remains a separate step.
    /// Backends should consume
    /// trigger, polarity, controller, source, affinity, and flags metadata that
    /// they understand, and ignore metadata that is not relevant to that
    /// controller. MSI descriptors are programmed through `MsiBackendIf` and do
    /// not use the normal line configure path.
    /// Must obey the same bounded, non-sleeping and non-reentrant controller
    /// contract as [`IntrManagerIf::enable`]; Line state may be locked here too.
    /// Returns an error if programming or required completion/readback fails.
    /// On failure KIRQ does not publish a new action or commit configuration,
    /// but retains resolved mappings and cannot undo partial hardware effects.
    /// The provider must not restore delivery after a failed configuration.
    /// If the source exposes immediate affinity operations, configuration must
    /// preserve the route established by those operations. Descriptor affinity
    /// metadata is not a replacement for the runtime routing policy.
    fn configure(desc: IrqDesc) -> Result<(), crate::IrqDescError>;

    /// Enables or disables a controller-local IRQ source.
    ///
    /// `desc` preserves the normalized domain and controller metadata of the
    /// selected Line. Its `hwirq` is the backend-local source id, not a dynamic
    /// virq. The descriptor is metadata, not a hardware-binding lifetime token.
    /// Enabling is normally called after [`IntrManagerIf::configure`].
    /// Disabling should mask or otherwise stop delivery of that local source.
    /// Infallible providers must diagnose observed completion failures; return
    /// alone is not proof of hardware quiescence or completed device shutdown.
    /// MSI enablement is represented by the device-visible MSI message and does
    /// not use this hook.
    ///
    /// Must be bounded and non-sleeping. KIRQ may hold the selected Line's
    /// IRQ-safe state lock across this call. The provider may acquire its own
    /// controller-register locks, but must not reenter KIRQ registry, control
    /// or dispatch paths while holding those locks or servicing this call.
    fn enable(desc: IrqDesc, on: bool);

    /// Claims, classifies, and dispatches through the ordinary IRQ entry.
    ///
    /// The interrupted context had ordinary IRQs enabled; this method itself
    /// runs with ordinary IRQs masked, migration excluded and generic hardirq
    /// context active. A controller may admit NMI nesting without admitting
    /// ordinary IRQs. It must return with ordinary IRQ exclusion intact.
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
    ///
    /// # Safety
    /// Enter only from the captured ordinary IRQ on an initialized CPU, with
    /// hardirq context and watching active and IRQ/migration exclusion retained
    /// until return. The vector must belong to that architecture entry.
    unsafe fn dispatch_irq(id: usize) -> Option<Virq>;

    /// Claims and dispatches an interrupt taken through the NMI entry path.
    ///
    /// The backend must call [`crate::generic_handle_nmi`] with a
    /// [`PendingIrq`] containing its typed source and completion cookie. Unlike
    /// [`IntrManagerIf::dispatch_irq`], this path never enters the normal IRQ
    /// lifecycle or runs its deferred-work tail. It runs on the initialized entry CPU
    /// with ordinary IRQs and migration excluded and effective NMI context
    /// already active. Neither dispatch nor completion may schedule.
    ///
    /// # Safety
    /// Enter only from an architecture-classified NMI with effective NMI
    /// context and watching active. Retain ordinary IRQ/migration exclusion
    /// on the initialized entry CPU throughout dispatch and completion.
    unsafe fn dispatch_nmi(id: usize);

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
    /// For selected Fasteoi Lines, KIRQ invokes this hook under the original
    /// Line state lock, after clearing its running ownership where applicable.
    /// Edge Lines invoke it under that lock before action execution (or after
    /// masking a rejected arrival), without clearing another running owner.
    /// Level Lines invoke it after mask/ack and before actions; a backend that
    /// consumed its parent acknowledgement already supplies no tail obligation.
    /// Normal-IRQ claims rejected for an NMI-owned Line also complete under
    /// that Line's lock, without invoking the NMI action. Actual NMI entry
    /// remains lockless and obeys the NMI-context restrictions above.
    /// Rejected PerCpu claims likewise complete under the Line lock without
    /// borrowing an action; accepted PerCpu execution completes unlocked.
    /// It must not reenter KIRQ dispatch, management, or registry operations:
    /// the permitted lock order is Line then bounded controller state.
    ///
    /// # Safety
    /// Consume one still-owned completion obligation from this controller on
    /// its claiming CPU, with ordinary IRQs/migration excluded. Do not reuse
    /// the cookie except for a backend-defined no-completion sentinel.
    unsafe fn complete_irq(completion_cookie: usize);

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
fn platform_configure(desc: IrqDesc) -> Result<(), crate::IrqDescError> {
    IntrManagerIf::configure(desc)
}

#[inline]
fn platform_enable(desc: IrqDesc, on: bool) {
    IntrManagerIf::enable(desc, on)
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
pub(crate) fn configure_platform_irq(desc: IrqDesc) -> Result<(), crate::IrqDescError> {
    if needs_platform_binding(desc) {
        platform_configure(desc)?;
    }
    Ok(())
}

#[inline]
pub(crate) fn set_platform_irq_enabled(desc: IrqDesc, on: bool) {
    if needs_platform_binding(desc) {
        platform_enable(desc, on);
    }
}

#[inline]
pub fn notify_cpu(id: usize, target: TargetCpu) {
    IntrManagerIf::notify_cpu(id, target)
}

#[inline]
pub fn set_prio(id: usize, prio: u8) {
    IntrManagerIf::set_prio(id, prio)
}
