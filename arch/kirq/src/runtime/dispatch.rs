// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regular IRQ dispatch and wake-subscription fanout.

use crate::{
    action::allows_wake_compat,
    platform::PendingIrq,
    state::{IRQ_STATE, WakeupMode},
};

/// Dispatches regular IRQ subscribers and returns the resolved logical IRQ.
///
/// The return value is for IRQ-tail context only. It does not mean a descriptor
/// existed or a handler serviced the interrupt; descriptor misses are still
/// reported as unhandled and return the resolved `virq` so diagnostics and
/// deferred hooks can identify the claimed line.
pub(super) fn dispatch_subscribers(pending: &PendingIrq) -> Option<crate::Virq> {
    let (virq, snapshot) = {
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
        let snapshot = entry.snapshot_dispatch();
        if snapshot.wake_subscription.is_none() {
            state.remove_if_unused(virq);
        }
        (virq, snapshot)
    };

    let mut can_run_wake_compat = true;
    let action_return = snapshot.regular_action.and_then(|action| {
        if action.is_currently_dispatchable() {
            Some(action.run_primary())
        } else {
            warn!("IRQ {virq} action has unsupported threaded state");
            can_run_wake_compat = false;
            None
        }
    });

    if can_run_wake_compat
        && allows_wake_compat(action_return)
        && let Some(wake_subscription) = snapshot.wake_subscription
    {
        if !snapshot.has_regular_action && wake_subscription.mode == WakeupMode::OneShot {
            super::manager::enable(snapshot.desc, false);
        }
        (wake_subscription.handler)(virq);
    } else if !snapshot.has_regular_action {
        warn!("Unhandled IRQ {virq}");
    }
    Some(virq)
}
