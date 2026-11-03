// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ-owned pending service, scheduled by a permanent non-sleeping BH executor.
//!
//! Queue ownership retains the original Line and contributes to its drain. No
//! hardware claim is created or completed by software service.

use alloc::sync::Arc;

use intrusive_collections::LinkedList;
use kspin::{SpinNoIrq, static_lock};

use super::{
    dispatch,
    state::{IrqLine, ReplayAdapter},
};

const BATCH_LIMIT: usize = 64;

struct ReplayQueue {
    pending: LinkedList<ReplayAdapter>,
    notify: Option<fn()>,
}

static_lock! {
    static REPLAY: SpinNoIrq<ReplayQueue> = SpinNoIrq::new(ReplayQueue {
        pending: LinkedList::new(ReplayAdapter::NEW),
        notify: None,
    });
}

/// Binds the permanent executor after worker and per-CPU services are ready.
///
/// Returns false on duplicate installation. Previously queued Lines are retained
/// and kicked after publication; startup must not wait for them before binding.
///
/// # Safety
/// `notify` must schedule `run` in non-sleeping BH context, never inline, and
/// must cover every notification (coalescing is allowed). It must be IRQ-safe,
/// allocation-free and kernel-lifetime, with no cancellation or service shutdown.
/// Call in serialized normal startup context, not NMI.
pub unsafe fn install(notify: fn()) -> bool {
    debug_assert!(
        !crate::context::is_in_interrupt_context(),
        "replay executor installation requires normal startup context"
    );
    // The queue lock serializes publication; startup ordering and the supplied
    // function's lifetime/non-inline execution cannot be checked here.
    let has_pending = {
        let mut queue = REPLAY.lock();
        if queue.notify.is_some() {
            return false;
        }
        queue.notify = Some(notify);
        !queue.pending.is_empty()
    };
    if has_pending {
        notify();
    }
    true
}

pub(super) fn schedule(line: &Arc<IrqLine>) {
    schedule_on(&REPLAY, line);
}

fn schedule_on(queue: &SpinNoIrq<ReplayQueue>, line: &Arc<IrqLine>) {
    let notify = {
        let mut entry = line.lock();
        if !entry.needs_replay() {
            return;
        }
        let chip = match entry.flow {
            crate::IrqFlow::Level(_) => {
                entry.is_pending = false;
                return;
            }
            crate::IrqFlow::Edge(chip) => {
                if (chip.retrigger)(entry.desc) {
                    entry.is_pending = false;
                }
                return;
            }
            crate::IrqFlow::Fasteoi(chip) => chip,
            crate::IrqFlow::Legacy | crate::IrqFlow::PerCpu => return,
        };
        // Only an explicit level contract permits relying on hardware
        // reassertion. Unknown-trigger PLIC sources still need software service.
        if matches!(chip.resend, crate::FasteoiResend::Level)
            || matches!(
                entry.desc.trigger,
                crate::IrqTrigger::LevelHigh | crate::IrqTrigger::LevelLow
            )
        {
            entry.is_pending = false;
            return;
        }
        if let crate::FasteoiResend::HardwareOnly(retrigger) = chip.resend {
            if retrigger(entry.desc) {
                entry.is_pending = false;
            }
            return;
        }
        entry.is_replay_queued = true;
        entry.begin_dispatch();
        // Lock order: Line then queue. Consumers release queue before Line.
        let mut queue = queue.lock();
        queue.pending.push_back(line.clone());
        queue.notify
    };
    if let Some(notify) = notify {
        notify();
    }
}

/// Services a bounded batch, re-notifying the executor when work remains.
///
/// Call only from the installed BH executor, outside its queue locks. Replays
/// run with local IRQs masked and never sleep. Concurrent invocations are safe:
/// the queue transfers a distinct owner and Line exclusion serializes admission.
/// Task/NMI/hardirq calls return without consuming work.
pub fn run() {
    if !crate::context::is_softirq_context() || crate::context::is_in_nmi() {
        return;
    }
    run_on(&REPLAY);
}

fn run_on(replay: &SpinNoIrq<ReplayQueue>) {
    for _ in 0..BATCH_LIMIT {
        let line = replay.lock().pending.pop_front();
        let Some(line) = line else {
            return;
        };
        dispatch::replay(line);
    }
    let notify = {
        let queue = replay.lock();
        if queue.pending.is_empty() {
            None
        } else {
            queue.notify
        }
    };
    if let Some(notify) = notify {
        notify();
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/resend.rs");
