// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regular IRQ dispatch and action fanout.
//!
//! Selects the bound flow, pairs action traversal with execution admission, and
//! orders the original claim's completion before releasing that admission.
//! Lookup retains the original Line identity; it does not authorize an action
//! after withdrawal. Registration and source shutdown remain outside this module.

use alloc::sync::Arc;
use core::marker::PhantomData;

use kcompletion::DeferredWake;
use kspin::{NoPreemptIrqSave, SpinNoIrqGuard};

use crate::{
    IrqEvent, IrqFlow, IrqRef,
    state::{IrqLine, IrqStateDesc},
};

/// Retains dispatch admission until the caller has completed the controller claim.
#[must_use = "complete the original controller claim before releasing dispatch admission"]
pub(super) struct IrqDispatch {
    resolved_irq: Option<crate::Virq>,
    guard: Option<IrqDispatchGuard>,
}

impl IrqDispatch {
    /// Completes the original claim before releasing this dispatch's admission.
    ///
    /// The caller supplies the original claim's completion, on the claiming CPU.
    /// A running fasteoi owner clears running and completes under its original
    /// Line lock. Accepted PerCpu completion retains its unlocked contract.
    pub(super) fn complete(mut self, complete_claim: impl FnOnce()) -> Option<crate::Virq> {
        let resolved_irq = self.resolved_irq;
        if let Some(guard) = self
            .guard
            .as_mut()
            .filter(|guard| guard.admission == DispatchAdmission::Running)
        {
            let mut entry = guard.line.lock();
            entry.is_running = false;
            entry.is_replaying = false;
            complete_with_deferred_affinity(&mut entry, complete_claim);
            // SAFETY: entry was locked directly through this retained Line.
            let notify = unsafe { super::resend::prepare_locked(&guard.line, &mut entry) };
            let finished = finish_admission(&mut entry, &mut guard.admission, guard.is_unhandled);
            drop(entry);
            report_finished(guard.virq, finished);
            if let Some(notify) = notify {
                notify();
            }
        } else {
            complete_claim();
        }
        drop(self);
        resolved_irq
    }

    /// Releases a software-only traversal that owns no hardware claim.
    ///
    /// Replay uses this separate path so an empty completion closure can never
    /// be confused with a real EOI. No deferred-affinity phase is invoked.
    pub(super) fn complete_software_only(mut self) -> Option<crate::Virq> {
        let resolved_irq = self.resolved_irq;
        if let Some(guard) = self
            .guard
            .as_mut()
            .filter(|guard| guard.admission == DispatchAdmission::Running)
        {
            let mut entry = guard.line.lock();
            entry.is_running = false;
            entry.is_replaying = false;
            // SAFETY: entry was locked directly through this retained Line.
            let notify = unsafe { super::resend::prepare_locked(&guard.line, &mut entry) };
            let finished = finish_admission(&mut entry, &mut guard.admission, guard.is_unhandled);
            drop(entry);
            report_finished(guard.virq, finished);
            if let Some(notify) = notify {
                notify();
            }
        }
        drop(self);
        resolved_irq
    }
}

/// Runs a real hardware completion with the deferred-affinity pre/post phases.
///
/// Callers must hold the Line lock and own an actual hardware claim. The helper
/// is not used for replay or for completions without a Line.
fn complete_with_deferred_affinity(entry: &mut IrqStateDesc, complete_claim: impl FnOnce()) {
    let token = super::affinity::prepare_deferred_affinity(entry);
    complete_claim();
    if let Some(token) = token {
        super::affinity::finish_deferred_affinity(entry, token);
    }
}

/// Dispatches a real claim and consumes its original completion on every path.
pub(super) fn dispatch_claim(source: IrqRef, complete_claim: impl FnOnce()) -> Option<crate::Virq> {
    let _context_guard = NoPreemptIrqSave::new();
    let Some((virq, line)) = super::index::lookup(source) else {
        warn!("Unhandled IRQ {source:?}");
        complete_claim();
        return None;
    };
    let Some(line) = line else {
        warn!("Unhandled IRQ {virq}");
        // A resolved source without a descriptor still completes its own claim.
        complete_claim();
        return Some(virq);
    };
    let entry = line.lock();
    if entry.is_retired() {
        drop(entry);
        // A stale lookup must complete its claim without touching the old chip.
        complete_claim();
        return Some(virq);
    }
    if entry.has_nmi_action() {
        // Classification belongs to the irqchip. Never execute an NMI action
        // under ordinary IRQ context or silently change its hardware-number ABI.
        // Keep completion under Line exclusion so drain and retirement cannot
        // pass it; rejection borrows no action and needs no execution credit.
        complete_claim();
        drop(entry);
        return Some(virq);
    }

    let Some(flow) = entry.flow else {
        complete_claim();
        return Some(virq);
    };
    match flow {
        IrqFlow::Level(chip) => {
            // SAFETY: lookup paired virq with line; entry comes from line.lock().
            unsafe { handle_level_irq(entry, &line, virq, chip, complete_claim) }
        }
        IrqFlow::Edge(chip) => {
            // SAFETY: lookup paired virq with line; entry comes from line.lock().
            unsafe { handle_edge_irq(entry, &line, virq, chip, complete_claim) }
        }
        IrqFlow::Fasteoi(chip) => {
            // SAFETY: lookup paired virq with line; entry comes from line.lock().
            unsafe { handle_fasteoi_irq(entry, &line, virq, chip, complete_claim) }
        }
        IrqFlow::PerCpu => {
            // SAFETY: lookup paired virq with line; entry comes from line.lock().
            unsafe { handle_percpu_irq(entry, &line, virq, complete_claim) }
        }
    }
    Some(virq)
}

// Ordinary rejection completes under Line exclusion, which also serializes
// drain and resource withdrawal. Accepted execution owns the running interval
// across the unlocked interval. Each caller passes this exact locked Line.
/// # Safety
/// `entry` must lock `line`, and `virq` must identify that same Line.
unsafe fn handle_level_irq(
    mut entry: SpinNoIrqGuard<'_, IrqStateDesc>,
    line: &Arc<IrqLine>,
    virq: crate::Virq,
    chip: &'static crate::LevelChip,
    complete_claim: impl FnOnce(),
) {
    (chip.mask_ack)(entry.desc);
    complete_with_deferred_affinity(&mut entry, complete_claim);
    if entry.is_running || !entry.can_run_actions() {
        entry.is_pending = true;
        return;
    }
    entry.begin_running_dispatch();
    let mut guard = IrqDispatchGuard::new_running(virq, line.clone());
    entry.is_pending = false;
    // SAFETY: dispatch_claim passed this exact locked Line. Admission and
    // capture occur under that lock; running ownership spans every snapshot use.
    let snapshot = unsafe { guard.snapshot_actions(&entry) };
    drop(entry);

    guard.is_unhandled = run_actions(guard.virq, snapshot);
    let mut entry = guard.line.lock();
    entry.is_running = false;
    if entry.can_run_actions() {
        // Level reassertion, not software replay, services a surviving cause.
        entry.is_pending = false;
        (chip.unmask)(entry.desc);
    }
    let finished = finish_admission(&mut entry, &mut guard.admission, guard.is_unhandled);
    drop(entry);
    report_finished(virq, finished);
}

/// # Safety
/// `entry` must lock `line`, and `virq` must identify that same Line.
unsafe fn handle_edge_irq(
    mut entry: SpinNoIrqGuard<'_, IrqStateDesc>,
    line: &Arc<IrqLine>,
    virq: crate::Virq,
    chip: &'static crate::EdgeChip,
    complete_claim: impl FnOnce(),
) {
    if entry.is_running || !entry.can_run_actions() {
        entry.is_pending = true;
        (chip.mask)(entry.desc);
        complete_with_deferred_affinity(&mut entry, complete_claim);
        return;
    }
    entry.begin_running_dispatch();
    let mut guard = IrqDispatchGuard::new_running(virq, line.clone());
    entry.is_pending = false;
    complete_with_deferred_affinity(&mut entry, complete_claim);
    // SAFETY: entry locks the exact Line retained by guard. Admission was
    // acquired under that lock and covers every iteration of this owner.
    let mut snapshot = unsafe { guard.snapshot_actions(&entry) };
    drop(entry);

    let mut is_unhandled = false;
    loop {
        is_unhandled |= run_actions(guard.virq, snapshot);
        let mut entry = guard.line.lock();
        if !entry.is_pending || !entry.can_run_actions() {
            let finished = finish_admission(&mut entry, &mut guard.admission, is_unhandled);
            drop(entry);
            report_finished(virq, finished);
            break;
        }
        // The running owner consumes coalesced edges, unless disabled/retired.
        (chip.unmask)(entry.desc);
        entry.is_pending = false;
        // SAFETY: entry was acquired directly from guard.line; the original
        // admission remains held across every coalesced-edge traversal.
        snapshot = unsafe { guard.snapshot_actions(&entry) };
    }
}

/// # Safety
/// `entry` must lock `line`, and `virq` must identify that same Line.
unsafe fn handle_fasteoi_irq(
    mut entry: SpinNoIrqGuard<'_, IrqStateDesc>,
    line: &Arc<IrqLine>,
    virq: crate::Virq,
    chip: &'static crate::FasteoiChip,
    complete_claim: impl FnOnce(),
) {
    if entry.is_running || !entry.can_run_actions() {
        if entry.is_replaying {
            // Replay owns no hardware claim: preserve arrivals even after its
            // last device read, for service after the replay owner exits.
            entry.is_pending = true;
        } else if !entry.is_running {
            entry.is_pending = true;
            (chip.mask)(entry.desc);
        }
        // Replay owns no hardware claim; release its admission without running
        // deferred hardware phases. A rejected real claim still performs EOI.
        if entry.is_replaying {
            complete_claim();
            return;
        }
        complete_with_deferred_affinity(&mut entry, complete_claim);
        return;
    }
    entry.begin_running_dispatch();
    let mut guard = IrqDispatchGuard::new_running(virq, line.clone());
    entry.is_pending = false;
    // SAFETY: entry locks the exact Line retained by guard. Admission was
    // acquired under that lock and remains held through original completion.
    let snapshot = unsafe { guard.snapshot_actions(&entry) };
    drop(entry);

    guard.is_unhandled = run_actions(guard.virq, snapshot);
    IrqDispatch {
        resolved_irq: Some(guard.virq),
        guard: Some(guard),
    }
    .complete(complete_claim);
}

fn handle_unlocked_actions(
    entry: SpinNoIrqGuard<'_, IrqStateDesc>,
    mut guard: IrqDispatchGuard,
    complete_claim: impl FnOnce(),
) {
    // SAFETY: the PerCpu caller forwards the original Line lock. The guard
    // retains that Line, and unsafe PerCpu removal requires the source owner
    // to exclude withdrawal until this entire dispatch returns.
    let snapshot = unsafe { guard.snapshot_actions(&entry) };
    drop(entry);
    guard.is_unhandled = run_actions(guard.virq, snapshot);
    IrqDispatch {
        resolved_irq: Some(guard.virq),
        guard: Some(guard),
    }
    .complete(complete_claim);
}

/// Runs the PerCpu flow in the IRQ dispatch path; sleeping is forbidden.
/// `dispatch_claim` retains IRQ/preemption exclusion and passes the held Line
/// SpinNoIrq guard after releasing the directory lock. Actions and controller
/// completion run unlocked only for accepted actions. Rejected claims complete
/// under Line exclusion without admission. The outer context guard remains held
/// throughout, keeping accepted admission on the claiming CPU.
///
/// # Safety
/// `entry` must lock `line`, and `virq` must identify that same Line.
unsafe fn handle_percpu_irq(
    entry: SpinNoIrqGuard<'_, IrqStateDesc>,
    line: &Arc<IrqLine>,
    virq: crate::Virq,
    complete_claim: impl FnOnce(),
) {
    if !entry.can_run_local_actions() {
        // No action is borrowed. The same Line lock used by drain and removal
        // retains the original completion obligation without an execution credit.
        complete_claim();
        return;
    }
    // The source owner excludes withdrawal until every local execution and
    // completion has returned. PerCpu flow has no shared running state.
    let guard = IrqDispatchGuard::new_percpu(virq, line.clone());
    handle_unlocked_actions(entry, guard, complete_claim);
}

fn run_actions(virq: crate::Virq, snapshot: super::state::IrqActionSnapshot<'_>) -> bool {
    let mut merged_event = IrqEvent::NOT_HANDLED;
    snapshot.with_actions(|action| {
        if action.is_currently_dispatchable() {
            let action_return = action.run_primary(virq);
            if action_return.handled() {
                merged_event.merge(IrqEvent::from_sources(action_return.sources()));
            }
        } else {
            warn!("IRQ {virq} action has unsupported threaded state");
        }
    });
    !merged_event.handled()
}

pub(super) fn replay(line: Arc<IrqLine>) {
    let _context_guard = NoPreemptIrqSave::new();
    let mut entry = line.lock();
    entry.is_replay_queued = false;
    #[expect(
        clippy::expect_used,
        reason = "CG-021-no-panic-on-reachable-failure: replay is queued only on a published, \
                  resolved Line"
    )]
    let virq = entry.desc.logical_irq().expect("replay Line has a virq");
    if !entry.is_pending || !entry.can_run_actions() || entry.is_running {
        // A hardware handler may already have serviced the condition, or the
        // owner may be withdrawing. No client snapshot is retained by the queue.
        return;
    }
    // Queue ownership protects the Line only. Pair action capture with
    // execution admission under the same lock as withdrawal and drain checks.
    entry.begin_running_dispatch();
    let mut guard = IrqDispatchGuard::new_running(virq, line.clone());
    entry.is_pending = false;
    entry.is_replaying = true;
    // SAFETY: entry locks the same retained queue Line cloned into guard; the
    // guard owns execution admission through replay traversal and completion.
    let snapshot = unsafe { guard.snapshot_actions(&entry) };
    drop(entry);
    guard.is_unhandled = run_actions(virq, snapshot);
    IrqDispatch {
        resolved_irq: Some(virq),
        guard: Some(guard),
    }
    .complete_software_only();
}

/// Distinguishes ordinary running ownership from source-owner-protected PerCpu
/// traversal. PerCpu is not a shared counter or an independently drainable lease.
#[derive(Clone, Copy, Eq, PartialEq)]
enum DispatchAdmission {
    PerCpu,
    Running,
    Finished,
}

/// Retains the exact Line and its traversal admission through flow completion.
/// The ordinary running owner releases waiters on exit; PerCpu lifetime instead
/// relies on the source owner's exclusion through the complete dispatch return.
struct IrqDispatchGuard {
    virq: crate::Virq,
    line: Arc<IrqLine>,
    is_unhandled: bool,
    admission: DispatchAdmission,
    task_local: PhantomData<*mut ()>,
}

impl IrqDispatchGuard {
    fn new_percpu(virq: crate::Virq, line: Arc<IrqLine>) -> Self {
        Self {
            virq,
            line,
            is_unhandled: false,
            admission: DispatchAdmission::PerCpu,
            task_local: PhantomData,
        }
    }

    fn new_running(virq: crate::Virq, line: Arc<IrqLine>) -> Self {
        let mut guard = Self::new_percpu(virq, line);
        guard.admission = DispatchAdmission::Running;
        guard
    }

    /// # Safety
    /// `entry` must be protected by this guard's exact retained Line lock, not
    /// another Line with the same numeric IRQ. This guard must own an admission
    /// on that Line: ordinary running ownership or a PerCpu execution protected
    /// from withdrawal by the source owner's shutdown protocol. Every snapshot
    /// access must precede the end of that execution.
    unsafe fn snapshot_actions<'a>(
        &'a self,
        entry: &IrqStateDesc,
    ) -> super::state::IrqActionSnapshot<'a> {
        debug_assert_eq!(entry.desc.logical_irq(), Some(self.virq));
        debug_assert!(match self.admission {
            DispatchAdmission::PerCpu => entry.flow == Some(IrqFlow::PerCpu),
            DispatchAdmission::Running => entry.is_running,
            DispatchAdmission::Finished => false,
        });
        // SAFETY: the caller guarantees exact-Line exclusion and admission;
        // numeric identity above is diagnostic only. The snapshot cannot
        // outlive its borrow of this guard. The caller also keeps the owned
        // ordinary admission active through traversal. PerCpu removal instead
        // requires caller-completed execution exclusion before reclaim.
        unsafe { entry.snapshot_actions() }
    }
}

impl Drop for IrqDispatchGuard {
    fn drop(&mut self) {
        if self.admission == DispatchAdmission::Finished
            || (self.admission == DispatchAdmission::PerCpu && !self.is_unhandled)
        {
            // A handled PerCpu claim has no execution state to release.
            return;
        }
        let finished = {
            let mut entry = self.line.lock();
            // Completion owns admission, not directory membership. The retained
            // Line serializes both the predicate and completion publication;
            // Ordinary descriptor withdrawal follows drain; PerCpu withdrawal
            // requires the owner to have already excluded this execution.
            finish_admission(&mut entry, &mut self.admission, self.is_unhandled)
        };
        report_finished(self.virq, finished);
    }
}

fn finish_admission(
    entry: &mut IrqStateDesc,
    admission: &mut DispatchAdmission,
    is_unhandled: bool,
) -> (
    Option<DeferredWake>,
    Option<super::unhandled::UnhandledIrqReport>,
) {
    let wake = match core::mem::replace(admission, DispatchAdmission::Finished) {
        DispatchAdmission::PerCpu => None,
        DispatchAdmission::Running => entry.finish_running_dispatch(),
        DispatchAdmission::Finished => return (None, None),
    };
    let report = is_unhandled
        .then(|| entry.record_unhandled_dispatch())
        .flatten();
    (wake, report)
}

fn report_finished(
    virq: crate::Virq,
    (wait_set, report): (
        Option<DeferredWake>,
        Option<super::unhandled::UnhandledIrqReport>,
    ),
) {
    wake_execution_idle(wait_set);
    if let Some(report) = report {
        warn!(
            "Unhandled IRQ {} (descriptor lifetime count={}, previous warnings suppressed={}, \
             count saturated={}; reports at powers of two and first saturation)",
            virq,
            report.count,
            report.suppressed,
            report.count == u64::MAX,
        );
    }
}

fn wake_execution_idle(wait_set: Option<DeferredWake>) {
    if let Some(wait_set) = wait_set {
        wait_set.wake();
    }
}

unittest::path_mod_test!(tests_line, "../../ut-tests/line_dispatch.rs");
unittest::path_mod_test!(tests_edge, "../../ut-tests/edge_dispatch.rs");
unittest::path_mod_test!(tests_level, "../../ut-tests/level_dispatch.rs");
