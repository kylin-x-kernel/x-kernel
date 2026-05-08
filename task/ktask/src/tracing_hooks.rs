// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Optional scheduler trace hooks (filled by `ktracing` when tracepoints are used).

use kspin::SpinNoIrq;

type WakeupFn = fn(u64);
type SwitchFn = fn(u64, u64);

struct Hooks {
    wakeup: Option<WakeupFn>,
    switch: Option<SwitchFn>,
}

static HOOKS: SpinNoIrq<Hooks> = SpinNoIrq::new(Hooks {
    wakeup: None,
    switch: None,
});

/// Called once from `ktracing` init. Before that, fires are no-ops.
pub fn register_sched_trace_hooks(wakeup: WakeupFn, switch_fn: SwitchFn) {
    let mut g = HOOKS.lock();
    g.wakeup = Some(wakeup);
    g.switch = Some(switch_fn);
}

pub(crate) fn fire_task_wakeup(woken_tid: u64) {
    if let Some(f) = HOOKS.lock().wakeup {
        f(woken_tid);
    }
}

pub(crate) fn fire_context_switch(prev_tid: u64, next_tid: u64) {
    if let Some(f) = HOOKS.lock().switch {
        f(prev_tid, next_tid);
    }
}
