// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Regular IRQ dispatch and wake-subscription fanout.

use crate::{
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
    let (virq, desc, regular_handler, wake_subscription) = {
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
        let desc = entry.desc;
        let regular_handler = entry.handler.clone();
        let wake_subscription = match entry.wake_subscription {
            Some(subscription) if subscription.mode == WakeupMode::Persistent => Some(subscription),
            Some(subscription) if subscription.armed => {
                entry.wake_subscription = None;
                Some(subscription)
            }
            Some(_) => {
                entry.wake_subscription = None;
                None
            }
            None => None,
        };
        if wake_subscription.is_none() {
            state.remove_if_unused(virq);
        }
        (virq, desc, regular_handler, wake_subscription)
    };
    let has_regular_handler = regular_handler.is_some();

    if let Some(handler) = regular_handler {
        let _ = handler.handle();
    }

    if let Some(wake_subscription) = wake_subscription {
        if !has_regular_handler && wake_subscription.mode == WakeupMode::OneShot {
            crate::manager::enable(desc, false);
        }
        (wake_subscription.handler)(virq);
    } else if !has_regular_handler {
        warn!("Unhandled IRQ {virq}");
    }
    Some(virq)
}
