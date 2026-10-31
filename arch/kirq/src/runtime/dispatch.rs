// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regular IRQ dispatch and action fanout.

use alloc::sync::Arc;

use kcompletion::DeferredWake;

use crate::{
    IrqEvent,
    platform::PendingIrq,
    state::{IRQ_STATE, IrqLine},
};

/// Retains dispatch admission until the caller has completed the controller claim.
#[must_use = "complete the original controller claim before releasing dispatch admission"]
pub(super) struct IrqDispatch {
    resolved_irq: Option<crate::Virq>,
    _guard: Option<IrqDispatchGuard>,
}

impl IrqDispatch {
    /// Completes the original claim before releasing this dispatch's admission.
    ///
    /// The caller supplies the original claim's completion, on the claiming CPU.
    /// No Line or registry lock is held while invoking it in the current flow.
    pub(super) fn complete(self, complete_claim: impl FnOnce()) -> Option<crate::Virq> {
        let resolved_irq = self.resolved_irq;
        complete_claim();
        drop(self);
        resolved_irq
    }
}

/// Dispatches regular IRQ actions and retains the resolved Line's admission.
///
/// Consume the returned admission through `IrqDispatch::complete`. Its
/// resolved IRQ is only metadata for the tail: descriptor misses still return
/// that number, without creating a Line or attaching to a later replacement.
pub(super) fn dispatch_actions(pending: &PendingIrq) -> IrqDispatch {
    let (virq, snapshot, mut dispatch_guard) = {
        let state = IRQ_STATE.lock();
        let Some(virq) = pending.resolve() else {
            warn!("Unhandled IRQ {:?}", pending.source());
            return IrqDispatch {
                resolved_irq: None,
                _guard: None,
            };
        };
        let Some(line) = state.descs.get(&virq) else {
            warn!("Unhandled IRQ {virq}");
            // The source resolved, but no descriptor is currently registered.
            // Return immediately after the single unhandled warning; the caller
            // still completes the platform claim and runs IRQ-tail deferred work.
            return IrqDispatch {
                resolved_irq: Some(virq),
                _guard: None,
            };
        };
        let mut entry = line.lock();
        let snapshot = entry.snapshot_actions();
        // Empty Lines still own a controller claim. Keep admission until EOI,
        // not merely until the last copied handler returns.
        entry.begin_dispatch();
        let dispatch_guard = IrqDispatchGuard::new(virq, line.clone());
        drop(entry);
        (virq, snapshot, dispatch_guard)
    };

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
    dispatch_guard.is_unhandled = !merged_event.handled();
    IrqDispatch {
        resolved_irq: Some(virq),
        _guard: Some(dispatch_guard),
    }
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
