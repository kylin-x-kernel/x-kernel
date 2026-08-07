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
    let (virq, snapshot, dispatch_guard, remove_waiters) = {
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
        let (dispatch_guard, remove_waiters) = if snapshot.has_primary_actions() {
            // The snapshot and in-flight mark must be committed under the same
            // `IRQ_STATE` lock. Once an action is copied out, `free_irq*()` may
            // remove it from the control plane but must still wait for this
            // guard before releasing driver-owned state.
            entry.begin_dispatch();
            (Some(IrqDispatchGuard::new(virq)), false)
        } else {
            // Without a dispatch guard there is no escaped handler snapshot;
            // this is the only hot-path case where cleanup can succeed here.
            let remove_waiters = state.remove_if_unused(virq).is_some();
            (None, remove_waiters)
        };
        (virq, snapshot, dispatch_guard, remove_waiters)
    };
    if remove_waiters {
        super::notify::remove_irq_waiters(virq);
    }

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
    if merged_event.handled() {
        super::notify::dispatch_irq_event_waiters(virq, merged_event.sources());
    }

    if !merged_event.handled() {
        warn!("Unhandled IRQ {virq}");
    }
    drop(dispatch_guard);
    Some(virq)
}

struct IrqDispatchGuard {
    virq: crate::Virq,
}

impl IrqDispatchGuard {
    const fn new(virq: crate::Virq) -> Self {
        Self { virq }
    }
}

impl Drop for IrqDispatchGuard {
    fn drop(&mut self) {
        let (wait_set, remove_waiters) = {
            let mut state = IRQ_STATE.lock();
            let wait_set = state
                .descs
                .get_mut(&self.virq)
                .and_then(|entry| entry.finish_dispatch());
            // This is the matching cleanup point for the last in-flight snapshot:
            // once the guard drops, a descriptor left empty by `free_irq*()` may now
            // be removable. Platform masking is handled by the control path.
            let removed_desc = state.remove_if_unused(self.virq);
            (wait_set, removed_desc.is_some())
        };
        wake_in_flight_zero(wait_set);
        if remove_waiters {
            super::notify::remove_irq_waiters(self.virq);
        }
    }
}

fn wake_in_flight_zero(wait_set: Option<PollSet>) {
    if let Some(wait_set) = wait_set {
        wait_set.wake();
    }
}
