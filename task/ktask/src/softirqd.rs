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
    fn needs_resched() -> bool {
        let current = crate::current_may_uninit();
        task_needs_resched(current.as_ref().map(|task| -> &TaskInner { task }))
    }

    fn wake_current_cpu() {
        let _guard = NoPreempt::new();
        let cpu_id = khal::percpu::this_cpu_id();
        if let Some(source) = SOFTIRQD_WAKE_SOURCES.get(cpu_id) {
            let _ = source.wake();
        }
    }
}

fn task_needs_resched(task: Option<&TaskInner>) -> bool {
    task.is_some_and(TaskInner::is_preempt_pending)
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

#[cfg(unittest)]
#[allow(missing_docs)]
mod tests {
    use alloc::boxed::Box;
    use core::{
        sync::atomic::{AtomicUsize, Ordering},
        task::{RawWaker, RawWakerVTable, Waker},
    };

    use unittest::{assert_eq, def_test};

    use super::*;

    static TEST_DRAIN_RUNS: AtomicUsize = AtomicUsize::new(0);

    fn resched_and_reraise_action() {
        if TEST_DRAIN_RUNS.fetch_add(1, Ordering::SeqCst) == 0 {
            crate::current().set_preempt_pending(true);
            kirq::softirq::raise_softirq(kirq::softirq::SoftirqVec::Block);
        }
    }

    unsafe fn waker_clone(data: *const ()) -> RawWaker {
        RawWaker::new(data, &WAKER_VTABLE)
    }

    unsafe fn waker_wake(data: *const ()) {
        // SAFETY: test wakers install a leaked, aligned `AtomicUsize` pointer.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn waker_wake_by_ref(data: *const ()) {
        // SAFETY: test wakers install a leaked, aligned `AtomicUsize` pointer.
        let counter = unsafe { &*(data as *const AtomicUsize) };
        counter.fetch_add(1, Ordering::SeqCst);
    }

    unsafe fn waker_drop(_data: *const ()) {}

    static WAKER_VTABLE: RawWakerVTable =
        RawWakerVTable::new(waker_clone, waker_wake, waker_wake_by_ref, waker_drop);

    fn make_waker(counter: &'static AtomicUsize) -> Waker {
        let raw = RawWaker::new(counter as *const _ as *const (), &WAKER_VTABLE);
        // SAFETY: the raw waker data pointer is the leaked `AtomicUsize` above
        // and the vtable only performs atomic increments on that allocation.
        unsafe { Waker::from_raw(raw) }
    }

    fn ensure_test_wake_source() -> PollSet {
        let cpu_id = khal::percpu::this_cpu_id();
        if let Some(source) = SOFTIRQD_WAKE_SOURCES.get(cpu_id) {
            return source;
        }

        let source = PollSet::new();
        if !SOFTIRQD_WAKE_SOURCES.install(cpu_id, source.clone()) {
            panic!("softirq daemon wake source was installed concurrently");
        }
        source
    }

    fn test_softirq_drain_action() {
        TEST_DRAIN_RUNS.fetch_add(1, Ordering::SeqCst);
    }

    #[def_test]
    fn test_softirq_resched_without_current_task_is_false() {
        assert_eq!(task_needs_resched(None), false);
    }

    #[def_test(serial)]
    fn test_softirq_resched_query_preserves_task_request() {
        let _guard = kspin::NoPreemptIrqSave::new();
        let task = crate::current();
        let previous = task.is_preempt_pending();
        task.set_preempt_pending(false);
        let before = kirq::softirq::SoftirqDaemonIf::needs_resched();
        task.set_preempt_pending(true);
        let after = kirq::softirq::SoftirqDaemonIf::needs_resched();
        let preserved = task.is_preempt_pending();
        task.set_preempt_pending(previous);

        assert_eq!(before, false);
        assert_eq!(after, true);
        assert_eq!(preserved, true);
    }

    #[def_test(serial)]
    fn test_softirq_resched_handoff_remains_drainable() {
        use kirq::softirq::{SoftirqRunResult, SoftirqVec};

        let _wake_gate = kirq::softirq::test_support::ScopedDaemonWakeGate::disabled();
        let _action = kirq::softirq::test_support::ScopedSoftirqAction::install(
            SoftirqVec::Block,
            resched_and_reraise_action,
        );
        TEST_DRAIN_RUNS.store(0, Ordering::SeqCst);

        let (result, calls, pending) = {
            let _guard = kspin::NoPreemptIrqSave::new();
            let task = crate::current();
            let previous = task.is_preempt_pending();
            kirq::softirq::raise_softirq(SoftirqVec::Block);
            let result = kirq::softirq::run_pending_softirqs();
            let calls = TEST_DRAIN_RUNS.load(Ordering::SeqCst);
            let pending = kirq::softirq::local_softirq_pending();
            task.set_preempt_pending(previous);
            (result, calls, pending)
        };

        assert_eq!(result, SoftirqRunResult::Deferred);
        assert_eq!(calls, 1);
        assert_eq!(
            pending & (1usize << SoftirqVec::Block.as_usize()),
            1usize << SoftirqVec::Block.as_usize()
        );

        drain_pending_softirqs_for_current_cpu();

        assert_eq!(TEST_DRAIN_RUNS.load(Ordering::SeqCst), 2);
    }

    #[def_test(serial)]
    fn test_softirq_daemon_provider_wakes_current_cpu_source() {
        let source = ensure_test_wake_source();
        let counter = Box::leak(Box::new(AtomicUsize::new(0)));
        let registration = source.register(&make_waker(counter)).unwrap();

        kirq::softirq::SoftirqDaemonIf::wake_current_cpu();

        assert_eq!(counter.load(Ordering::SeqCst), 1);
        assert!(!registration.cancel());
    }

    #[def_test(serial)]
    fn test_softirq_daemon_drain_runs_pending_current_cpu_work() {
        let vec = kirq::softirq::SoftirqVec::Block;
        let _wake_gate = kirq::softirq::test_support::ScopedDaemonWakeGate::disabled();
        let _action = kirq::softirq::test_support::ScopedSoftirqAction::install(
            vec,
            test_softirq_drain_action,
        );

        TEST_DRAIN_RUNS.store(0, Ordering::SeqCst);
        kirq::softirq::raise_softirq(vec);

        drain_pending_softirqs_for_current_cpu();

        assert_eq!(TEST_DRAIN_RUNS.load(Ordering::SeqCst), 1);
        assert_eq!(
            kirq::softirq::local_softirq_pending() & (1usize << vec.as_usize()),
            0
        );
    }
}
