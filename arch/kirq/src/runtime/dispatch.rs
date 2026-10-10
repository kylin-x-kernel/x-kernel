// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regular IRQ dispatch and action fanout.

use kpoll::PollSet;

use crate::{IrqEvent, platform::PendingIrq, state::IRQ_STATE};

/// Dispatches regular IRQ actions and returns the resolved logical IRQ.
///
/// The return value is for IRQ-tail context only. It does not mean a descriptor
/// existed or a handler serviced the interrupt; descriptor misses are still
/// reported as unhandled and return the resolved `virq` so diagnostics and
/// deferred hooks can identify the claimed line.
pub(super) fn dispatch_actions(pending: &PendingIrq) -> Option<crate::Virq> {
    let (virq, snapshot, mut dispatch_guard) = {
        let mut state = IRQ_STATE.lock();
        let Some(virq) = pending.resolve() else {
            warn!("Unhandled IRQ {:?}", pending.source());
            return None;
        };
        let Some(entry) = state.descs.get_mut(&virq) else {
            warn!("Unhandled IRQ {virq}");
            // The source resolved, but no descriptor is currently registered.
            // Return immediately after the single unhandled warning; the caller
            // still completes the platform claim and runs IRQ-tail deferred work.
            return Some(virq);
        };
        let snapshot = entry.snapshot_actions();
        let dispatch_guard = if snapshot.has_primary_actions() {
            // The snapshot and in-flight mark must be committed under the same
            // `IRQ_STATE` lock. Once an action is copied out, `free_irq*()` may
            // remove it from the control plane but must still wait for this
            // guard before releasing driver-owned state.
            entry.begin_dispatch();
            Some(IrqDispatchGuard::new(virq))
        } else {
            // Without a dispatch guard there is no escaped handler snapshot;
            // this is the only hot-path case where cleanup can succeed here.
            state.remove_if_unused(virq);
            None
        };
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
    if let Some(guard) = dispatch_guard.as_mut() {
        guard.is_unhandled = !merged_event.handled();
    } else if !merged_event.handled() {
        warn!("Unhandled IRQ {virq}");
    }
    drop(dispatch_guard);
    Some(virq)
}

struct IrqDispatchGuard {
    virq: crate::Virq,
    is_unhandled: bool,
}

impl IrqDispatchGuard {
    const fn new(virq: crate::Virq) -> Self {
        Self {
            virq,
            is_unhandled: false,
        }
    }
}

impl Drop for IrqDispatchGuard {
    fn drop(&mut self) {
        let (wait_set, report) = {
            let mut state = IRQ_STATE.lock();
            let (wait_set, report) = if let Some(entry) = state.descs.get_mut(&self.virq) {
                // The in-flight mark keeps this descriptor alive across action
                // changes. Count only after the complete shared chain returned,
                // using the existing cleanup lock rather than another hot-path lock.
                let report = if self.is_unhandled {
                    entry.record_unhandled_dispatch()
                } else {
                    None
                };
                (entry.finish_dispatch(), report)
            } else {
                (None, None)
            };
            // This is the matching cleanup point for the last in-flight snapshot:
            // once the guard drops, a descriptor left empty by `free_irq*()` may now
            // be removable. Platform masking is handled by the control path.
            state.remove_if_unused(self.virq);
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

fn wake_in_flight_zero(wait_set: Option<PollSet>) {
    if let Some(wait_set) = wait_set {
        wait_set.wake();
    }
}
