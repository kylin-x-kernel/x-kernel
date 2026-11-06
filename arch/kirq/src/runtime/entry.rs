// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Architecture and irqchip entry orchestration.
//!
//! Orders RCU watching, IRQ/NMI context, controller dispatch and completion,
//! lifecycle hooks, and deferred IRQ-exit work. Ordinary flow execution belongs
//! to dispatch; registration, source control and retirement belong to manager.
//! The architecture adapter supplies IRQ/migration exclusion before entry.
//! Claim processing retains both; after completion, deferred work may briefly
//! enable IRQs while remaining CPU-pinned, then restores the entry mask.

use krcu_core::RcuContextIf;

use super::{
    dispatch::dispatch_claim,
    nmi::dispatch_nmi_handler,
    rcu::{with_nmi_claim, with_nmi_context},
};
use crate::{
    Virq,
    context::HardIrqContextGuard,
    lifecycle::IrqLifecycleGuard,
    platform::{IntrManagerIf, PendingIrq},
};

/// Handles a dedicated NMI entry after an architecture trap adapter has disabled
/// preemption.
///
/// Invoked when architecture classification selects the dedicated NMI path,
/// including a pseudo-NMI interrupting a PMR-masked critical section or a
/// hardware NMI bypassing the interrupted context's ordinary IRQ mask.
/// Dispatch borrows the
/// typed Domain index inside one RCU scope and the locally published action,
/// without acquiring registry, Line or management locks.
///
/// # Safety
///
/// `vector` must come from an architecture-classified NMI entry, not an
/// arbitrary software invocation or an ordinary interrupt mislabeled as NMI.
/// The adapter must enter on an initialized CPU with ordinary IRQs and migration
/// excluded, retaining that exclusion until return. Dispatch and completion must
/// finish on this stack without scheduling. This function does not establish
/// those entry conditions.
pub unsafe fn handle_nmi(vector: usize) -> bool {
    handle_nmi_with(vector, |id| {
        // SAFETY: caller supplies NMI entry exclusion; with_nmi_context adds
        // effective NMI context and watching before this provider invocation.
        unsafe { IntrManagerIf::dispatch_nmi(id) };
    })
}

fn handle_nmi_with(vector: usize, dispatch_fn: impl FnOnce(usize)) -> bool {
    // SAFETY: the architecture adapter excludes ordinary IRQs and migration;
    // controller dispatch completes all claims before returning to this stack.
    unsafe { with_nmi_context(|| dispatch_fn(vector)) };
    true
}

/// Dispatches and completes a claimed normal IRQ.
///
/// This is the generic entry for an irqchip that has classified its claim as a
/// normal IRQ. It resolves the claim, runs regular action fanout, and completes
/// the controller claim. Its return value is only the resolved normal `virq`
/// metadata used by the outer IRQ-exit deferred hook.
/// Accepted ordinary handlers take execution admission before snapshot/unlock.
/// Level/Edge/Fasteoi rejections instead hold the Line lock through chip operations
/// and original completion without an in-flight credit. Drain checks zero under
/// that same lock. Rejected normal claims for NMI-owned Lines likewise complete
/// under Line exclusion without execution admission, as do rejected PerCpu
/// claims. PerCpu execution and unlocked completion are excluded from removal
/// by the source owner's shutdown protocol, not a shared execution count.
/// Unbound descriptors never execute actions but still complete claims.
/// An NMI-owned Line never invokes its handler through this normal IRQ entry.
/// The irqchip must classify NMI claims and use [`generic_handle_nmi`].
///
/// The claim constructor establishes the current-CPU and entry-context
/// obligations through consumption. If a dedicated NMI provider mistakenly
/// selects this entry, the claim is completed without entering normal flow or
/// taking its locks. NMI-class claims must use [`generic_handle_nmi`].
#[doc(hidden)]
pub fn generic_handle_irq(claim: PendingIrq) -> Option<Virq> {
    if crate::context::is_in_nmi() {
        return None;
    }
    dispatch_and_complete(claim, PendingIrq::complete)
}

fn dispatch_and_complete(claim: PendingIrq, complete_irq: impl FnOnce(PendingIrq)) -> Option<Virq> {
    dispatch_claim(claim.source(), || complete_irq(claim))
}

/// Dispatches and completes a claimed NMI without touching normal IRQ state.
///
/// This is the generic entry for an irqchip that classifies an IRQ-on claim as
/// NMI-class. The typed source selects a Line through its Domain index; the
/// handler receives that Line's logical IRQ, just as normal IRQ handlers do. It bypasses
/// normal IRQ action fanout. [`handle_nmi`] also uses
/// this helper for an interrupt that entered through the architecture's
/// dedicated NMI path.
///
/// The claim constructor establishes same-CPU IRQ/NMI entry and exclusion
/// through consumption. This entry establishes effective NMI context itself.
/// A single interrupt RCU read scope covers handler dispatch and completion;
/// neither the scope nor effective NMI context ends at handler return.
#[doc(hidden)]
pub fn generic_handle_nmi(claim: PendingIrq) {
    let source = claim.source();
    // SAFETY: PendingIrq construction requires an initialized entry CPU and
    // IRQ/migration exclusion through consumption. The NMI scope includes
    // completion; neither operation schedules or lets the claim escape.
    unsafe {
        with_nmi_claim(
            |scope| dispatch_nmi_handler(source, scope),
            || claim.complete(),
        )
    };
}

/// Handles a normal IRQ after an architecture trap adapter has masked local
/// IRQs and disabled preemption.
///
/// Invoked when the interrupted context had normal IRQs enabled. PMR-based
/// classification is specific to AArch64 pseudo-NMI, not a generic prerequisite.
/// The caller must keep the current CPU pinned for this call. Claim processing
/// keeps ordinary IRQs masked, including when the irqchip permits NMI nesting.
/// After all claims complete, the deferred tail may open ordinary IRQ windows;
/// it restores the masked state before watching exit and return.
///
/// # Safety
///
/// Enter on an initialized CPU with preemption disabled and ordinary IRQs
/// masked; retain CPU pinning until return and permit the deferred-tail IRQ
/// windows described above. `vector` must identify the
/// captured architecture interrupt. Callers must not hold locks needed by
/// controller dispatch, lifecycle/deferred hooks or driver IRQ handlers.
/// A synthetic or hypervisor adapter would have to establish the same complete
/// entry contract; being in a VM-exit path alone does not establish it.
/// `handle_irq()` itself is non-sleepable.
///
/// The irqchip classifies each successful claim and calls either
/// [`generic_handle_irq`] or [`generic_handle_nmi`]. For a claimed normal IRQ,
/// the generic ordering is:
///
/// 1. establish RCU watching, then enter lifecycle hooks and hardirq context;
/// 2. claim, ack, and classify through [`crate::IntrManagerIf::dispatch_irq`];
/// 3. invoke the irqchip-selected generic IRQ or NMI helper;
/// 4. complete the controller claim inside that helper;
/// 5. leave generic hardirq context;
/// 6. run the common IRQ-exit deferred hook;
/// 7. leave lifecycle hooks, then finish the watching entry before returning
///    to the HAL scheduling tail.
///
/// As on Linux, an NMI discovered after entering from an IRQ-on context remains
/// inside the outer IRQ lifecycle. Only its inner handler dispatch differs:
/// it borrows the Line-owned NMI action rather than normal IRQ action state.
pub unsafe fn handle_irq(vector: usize) -> bool {
    handle_irq_with(vector, |id| {
        // SAFETY: caller supplies IRQ/migration exclusion at entry; the helper
        // establishes watching/hardirq context and invokes the provider before
        // the deferred tail can open any ordinary IRQ window.
        unsafe { IntrManagerIf::dispatch_irq(id) }
    })
}

fn handle_irq_with(vector: usize, dispatch_fn: impl FnOnce(usize) -> Option<Virq>) -> bool {
    // SAFETY: the HAL adapter enters with IRQs masked and pins this CPU through
    // dispatch and deferred work. End watching entry before its scheduling tail.
    let interrupt_entry = unsafe { RcuContextIf::current_cpu_watching().enter_interrupt() };
    let lifecycle_guard = IrqLifecycleGuard::enter();
    let hardirq_context_guard = HardIrqContextGuard::enter();

    // The irqchip consumes every successful claim through one of the generic
    // helpers before returning. What remains here is the common outer
    // IRQ-entry tail, including IRQ-on NMI and spurious-claim cases.
    let resolved_irq = dispatch_fn(vector);
    run_irq_exit_tail_with_context(vector, resolved_irq, hardirq_context_guard);

    drop(lifecycle_guard);
    // SAFETY: controller, nested entries, deferred work and lifecycle hooks
    // have finished without scheduling; deferred work restored IRQ exclusion
    // and the HAL's CPU pin is still held.
    unsafe { interrupt_entry.exit() };
    true
}

unittest::path_mod_test!(tests_rcu_entry, "../../ut-tests/rcu_irq_entry.rs");

#[cfg(unittest)]
pub(super) fn dispatch_simulated_irq(claim: PendingIrq) -> Option<Virq> {
    assert!(claim.has_no_completion_for_tests());
    let _pin = kspin::NoPreemptIrqSave::new();
    let _hardirq = HardIrqContextGuard::enter();
    // Exercise the public consumer without creating a hardware obligation.
    generic_handle_irq(claim)
}

#[cfg(unittest)]
pub(super) fn run_claimed_irq_tail(
    vector: usize,
    pending_irq: PendingIrq,
    complete_irq: impl FnOnce(PendingIrq),
) {
    let hardirq_context_guard = HardIrqContextGuard::enter();
    let resolved_irq = dispatch_and_complete(pending_irq, complete_irq);
    run_irq_exit_tail_with_context(vector, resolved_irq, hardirq_context_guard);
}

fn run_irq_exit_tail_with_context(
    vector: usize,
    resolved_irq: Option<Virq>,
    hardirq_context_guard: HardIrqContextGuard,
) {
    drop(hardirq_context_guard);
    crate::deferred::run_hardirq_exit_deferred(crate::deferred::DeferredRunContext::new(
        vector,
        resolved_irq,
    ));
}

// Keep manager's existing fixture paths without exposing production helpers.
#[cfg(unittest)]
pub(super) fn dispatch_and_complete_for_tests(
    claim: PendingIrq,
    complete_irq: impl FnOnce(PendingIrq),
) -> Option<Virq> {
    dispatch_and_complete(claim, complete_irq)
}

#[cfg(unittest)]
pub(super) fn run_irq_exit_tail_for_tests(
    vector: usize,
    resolved_irq: Option<Virq>,
    hardirq_context_guard: HardIrqContextGuard,
) {
    run_irq_exit_tail_with_context(vector, resolved_irq, hardirq_context_guard);
}
