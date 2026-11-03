// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regular IRQ dispatch and action fanout.

use alloc::sync::Arc;

use kcompletion::DeferredWake;
use kspin::{NoPreemptIrqSave, SpinNoIrqGuard};

use crate::{
    IrqEvent, IrqFlow, IrqRef,
    state::{IRQ_STATE, IrqLine, IrqStateDesc},
};

/// Retains dispatch admission until the caller has completed the controller claim.
#[must_use = "complete the original controller claim before releasing dispatch admission"]
pub(super) struct IrqDispatch {
    resolved_irq: Option<crate::Virq>,
    guard: Option<IrqDispatchGuard>,
    owns_running: bool,
}

impl IrqDispatch {
    /// Completes the original claim before releasing this dispatch's admission.
    ///
    /// The caller supplies the original claim's completion, on the claiming CPU.
    /// A running fasteoi owner clears running and completes under its original
    /// Line lock. Legacy completion retains its existing unlocked contract.
    pub(super) fn complete(self, complete_claim: impl FnOnce()) -> Option<crate::Virq> {
        let resolved_irq = self.resolved_irq;
        if let Some(guard) = self.guard.as_ref().filter(|_| self.owns_running) {
            let mut entry = guard.line.lock();
            entry.is_running = false;
            entry.is_replaying = false;
            complete_claim();
            drop(entry);
            super::resend::schedule(&guard.line);
        } else {
            complete_claim();
        }
        drop(self);
        resolved_irq
    }
}

/// Dispatches a real claim and consumes its original completion on every path.
pub(super) fn dispatch_claim(source: IrqRef, complete_claim: impl FnOnce()) -> Option<crate::Virq> {
    let _context_guard = NoPreemptIrqSave::new();
    let state = IRQ_STATE.lock();
    let resolved = match source {
        IrqRef::Virq(virq) => Some(virq),
        IrqRef::Domain(domain, hwirq) => crate::domain::resolve(domain, hwirq),
    };
    let Some(virq) = resolved else {
        drop(state);
        warn!("Unhandled IRQ {source:?}");
        complete_claim();
        return None;
    };
    let Some(line) = state.descs.get(&virq) else {
        drop(state);
        warn!("Unhandled IRQ {virq}");
        // A resolved source without a descriptor still completes its own claim.
        complete_claim();
        return Some(virq);
    };
    let line = line.clone();
    let mut entry = line.lock();
    // Empty Lines retain admission through completion, not just action return.
    entry.begin_dispatch();
    let guard = IrqDispatchGuard::new(virq, line.clone());
    // The outer context guard preserves IRQ exclusion across this handoff.
    drop(state);

    match entry.flow {
        IrqFlow::Level(chip) => handle_level_irq(entry, guard, chip, complete_claim),
        IrqFlow::Edge(chip) => handle_edge_irq(entry, guard, chip, complete_claim),
        IrqFlow::Fasteoi(chip) => handle_fasteoi_irq(entry, guard, chip, complete_claim),
        IrqFlow::Legacy => handle_legacy_irq(entry, guard, complete_claim),
    }
    Some(virq)
}

// Each flow consumes the already-held Line guard; admission never crosses an
// unlocked gap. Release that guard before actions or dispatch-guard destruction.
fn handle_level_irq(
    mut entry: SpinNoIrqGuard<'_, IrqStateDesc>,
    mut guard: IrqDispatchGuard,
    chip: &'static crate::LevelChip,
    complete_claim: impl FnOnce(),
) {
    (chip.mask_ack)(entry.desc);
    complete_claim();
    if entry.is_running || !entry.can_run_actions() {
        entry.is_pending = true;
        drop(entry);
        drop(guard);
        return;
    }
    entry.is_running = true;
    entry.is_pending = false;
    let snapshot = entry.snapshot_actions();
    drop(entry);

    guard.is_unhandled = run_actions(guard.virq, snapshot);
    let mut entry = guard.line.lock();
    entry.is_running = false;
    if entry.can_run_actions() {
        // Level reassertion, not software replay, services a surviving cause.
        entry.is_pending = false;
        (chip.unmask)(entry.desc);
    }
    drop(entry);
}

fn handle_edge_irq(
    mut entry: SpinNoIrqGuard<'_, IrqStateDesc>,
    mut guard: IrqDispatchGuard,
    chip: &'static crate::EdgeChip,
    complete_claim: impl FnOnce(),
) {
    if entry.is_running || !entry.can_run_actions() {
        entry.is_pending = true;
        (chip.mask)(entry.desc);
        complete_claim();
        drop(entry);
        drop(guard);
        return;
    }
    entry.is_running = true;
    entry.is_pending = false;
    complete_claim();
    let mut snapshot = entry.snapshot_actions();
    drop(entry);

    loop {
        guard.is_unhandled |= run_actions(guard.virq, snapshot);
        let mut entry = guard.line.lock();
        if !entry.is_pending || !entry.can_run_actions() {
            entry.is_running = false;
            break;
        }
        // The running owner consumes coalesced edges, unless disabled/retired.
        (chip.unmask)(entry.desc);
        entry.is_pending = false;
        snapshot = entry.snapshot_actions();
    }
}

fn handle_fasteoi_irq(
    mut entry: SpinNoIrqGuard<'_, IrqStateDesc>,
    mut guard: IrqDispatchGuard,
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
        // Reject without clearing another dispatch's running ownership.
        complete_claim();
        drop(entry);
        drop(guard);
        return;
    }
    entry.is_running = true;
    entry.is_pending = false;
    let snapshot = entry.snapshot_actions();
    drop(entry);

    guard.is_unhandled = run_actions(guard.virq, snapshot);
    IrqDispatch {
        resolved_irq: Some(guard.virq),
        guard: Some(guard),
        owns_running: true,
    }
    .complete(complete_claim);
}

fn handle_legacy_irq(
    mut entry: SpinNoIrqGuard<'_, IrqStateDesc>,
    mut guard: IrqDispatchGuard,
    complete_claim: impl FnOnce(),
) {
    let snapshot = entry.snapshot_actions();
    drop(entry);
    guard.is_unhandled = run_actions(guard.virq, snapshot);
    IrqDispatch {
        resolved_irq: Some(guard.virq),
        guard: Some(guard),
        owns_running: false,
    }
    .complete(complete_claim);
}

fn run_actions(virq: crate::Virq, snapshot: super::state::IrqActionSnapshot) -> bool {
    let mut merged_event = IrqEvent::NOT_HANDLED;
    for action in snapshot.actions.into_iter().flatten() {
        if action.is_currently_dispatchable() {
            let action_return = action.run_primary(virq);
            if action_return.handled() {
                merged_event.merge(IrqEvent::from_sources(action_return.sources()));
            }
        } else {
            warn!("IRQ {virq} action has unsupported threaded state");
        }
    }
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
    let mut guard = IrqDispatchGuard::new(virq, line.clone());
    if !entry.is_pending || !entry.can_run_actions() || entry.is_running {
        // A hardware handler may already have serviced the condition, or the
        // owner may be withdrawing. No client snapshot is retained by the queue.
        drop(entry);
        drop(guard);
        return;
    }
    entry.is_pending = false;
    entry.is_running = true;
    entry.is_replaying = true;
    let snapshot = entry.snapshot_actions();
    drop(entry);
    guard.is_unhandled = run_actions(virq, snapshot);
    IrqDispatch {
        resolved_irq: Some(virq),
        guard: Some(guard),
        owns_running: true,
    }
    .complete(|| {});
}

struct IrqDispatchGuard {
    virq: crate::Virq,
    line: Arc<IrqLine>,
    is_unhandled: bool,
}

impl IrqDispatchGuard {
    fn new(virq: crate::Virq, line: Arc<IrqLine>) -> Self {
        Self {
            virq,
            line,
            is_unhandled: false,
        }
    }
}

impl Drop for IrqDispatchGuard {
    fn drop(&mut self) {
        let (wait_set, report) = {
            let mut state = IRQ_STATE.lock();
            let (wait_set, report) = {
                let mut entry = self.line.lock();
                // Account on the retained Line after the shared chain returned,
                // in the same state critical section as in-flight completion.
                let report = if self.is_unhandled {
                    entry.record_unhandled_dispatch()
                } else {
                    None
                };
                (entry.finish_dispatch(), report)
            };
            // The original controller claim has completed. An empty Line may
            // now be removable; platform masking belongs to the control path.
            if state
                .descs
                .get(&self.virq)
                .is_some_and(|line| Arc::ptr_eq(line, &self.line))
            {
                state.remove_if_unused(self.virq);
            }
            (wait_set, report)
        };
        wake_in_flight_zero(wait_set);
        if let Some(report) = report {
            warn!(
                "Unhandled IRQ {} (descriptor lifetime count={}, previous warnings suppressed={}, \
                 count saturated={}; reports at powers of two and first saturation)",
                self.virq,
                report.count,
                report.suppressed,
                report.count == u64::MAX,
            );
        }
    }
}

fn wake_in_flight_zero(wait_set: Option<DeferredWake>) {
    if let Some(wait_set) = wait_set {
        wait_set.wake();
    }
}

unittest::path_mod_test!(tests_line, "../../ut-tests/line_dispatch.rs");
unittest::path_mod_test!(tests_edge, "../../ut-tests/edge_dispatch.rs");
unittest::path_mod_test!(tests_level, "../../ut-tests/level_dispatch.rs");
