// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Per-CPU softirq daemon provider.

use alloc::{format, string::String};
use core::{future::poll_fn, task::Poll};

use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use kpoll::{PollRegistrations, PollSet};
use kspin::{NoPreempt, SpinNoIrq};

use crate::{KCpuMask, TaskInner, activate_task, future::block_on, prepare_task, yield_now};

static SOFTIRQD_WAKE_SOURCES: SoftirqDaemonWakeSources = SoftirqDaemonWakeSources::new();

struct SoftirqDaemonWakeSources([SpinNoIrq<Option<PollSet>>; kbuild_config::NR_CPUS]);

impl SoftirqDaemonWakeSources {
    const fn new() -> Self {
        Self([const { SpinNoIrq::new(None) }; kbuild_config::NR_CPUS])
    }

    fn install(&self, cpu_id: LogicalCpuId, source: PollSet) -> bool {
        let Some(cpu_slot) = self.0.get(cpu_id.as_usize()) else {
            warn!(
                "cannot install ksoftirqd wake source for out-of-range CPU {}",
                cpu_id.as_usize()
            );
            return false;
        };

        let mut slot = cpu_slot.lock();
        if slot.is_some() {
            return false;
        }
        *slot = Some(source);
        true
    }

    fn get(&self, cpu_id: LogicalCpuId) -> Option<PollSet> {
        self.0
            .get(cpu_id.as_usize())
            .and_then(|slot| slot.lock().clone())
    }
}

/// Starts the current CPU's softirq daemon if it has not been started yet.
///
/// The daemon is pinned to the current CPU because KIRQ softirq pending state is
/// per-CPU. Call this after scheduler bring-up and before enabling local IRQs
/// on each CPU.
pub fn init_current_cpu() {
    let cpu_id = khal::percpu::this_cpu_id();
    let wake_source = PollSet::new();
    if !SOFTIRQD_WAKE_SOURCES.install(cpu_id, wake_source.clone()) {
        return;
    }

    let task = TaskInner::new_pidless_kthread(
        move || softirqd_main(cpu_id, wake_source),
        daemon_name(cpu_id),
        kbuild_config::TASK_STACK_SIZE,
    );
    let task = prepare_task(task);
    task.set_cpumask(KCpuMask::one_shot_logical(cpu_id));
    activate_task(&task);
}

#[kiface::provide]
impl kirq::softirq::SoftirqDaemonIf {
    fn wake_current_cpu() {
        let _guard = NoPreempt::new();
        let cpu_id = khal::percpu::this_cpu_id();
        if let Some(source) = SOFTIRQD_WAKE_SOURCES.get(cpu_id) {
            let _ = source.wake();
        }
    }
}

fn softirqd_main(cpu_id: LogicalCpuId, wake_source: PollSet) {
    debug!("started {}", daemon_name(cpu_id));
    loop {
        wait_for_pending_softirqs(&wake_source);
        drain_pending_softirqs_for_current_cpu();
    }
}

fn drain_pending_softirqs_for_current_cpu() {
    match kirq::softirq::run_pending_softirqs() {
        kirq::softirq::SoftirqRunResult::NoPending => {}
        kirq::softirq::SoftirqRunResult::Ran | kirq::softirq::SoftirqRunResult::Deferred => {
            yield_now();
        }
    }
}

fn wait_for_pending_softirqs(wake_source: &PollSet) {
    enum WaitResult {
        PendingReady,
        RetryAfterYield,
    }

    let mut registrations = PollRegistrations::new();
    loop {
        match block_on(poll_fn(|cx| {
            if kirq::softirq::local_softirq_pending() != 0 {
                return Poll::Ready(WaitResult::PendingReady);
            }

            let mut context = registrations.context(cx);
            if let Err(error) = context.register(wake_source) {
                warn!("failed to register ksoftirqd waiter: {error:?}");
                drop(context);
                return Poll::Ready(WaitResult::RetryAfterYield);
            }
            drop(context);

            if kirq::softirq::local_softirq_pending() != 0 {
                return Poll::Ready(WaitResult::PendingReady);
            }
            Poll::Pending
        })) {
            WaitResult::PendingReady => break,
            WaitResult::RetryAfterYield => yield_now(),
        }
    }
}

fn daemon_name(cpu_id: LogicalCpuId) -> String {
    format!("ksoftirqd/{}", cpu_id.as_usize())
}

unittest::path_mod_test!(tests, "../ut-tests/softirqd/tests.rs");
