// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Reader enrollment and storage lifetime across scheduler transitions.

use alloc::sync::Arc;
use core::{
    any::Any,
    sync::atomic::{AtomicBool, Ordering},
};

use kalloc_types::KBox;
use kcpu_id_map::{KCpuMaskExt, LogicalCpuId};
use unittest::{assert, assert_eq, def_test};

struct RuntimeDropProbe(Arc<AtomicBool>);

impl crate::UserTaskRuntime for RuntimeDropProbe {
    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for RuntimeDropProbe {
    fn drop(&mut self) {
        self.0.store(true, Ordering::Release);
    }
}

struct FirstEntryProbe {
    outgoing: crate::WeakKtaskRef,
    saw_abandoned_reader: Arc<AtomicBool>,
    has_entered: AtomicBool,
    is_reclaimed: Arc<AtomicBool>,
}

impl crate::UserTaskRuntime for FirstEntryProbe {
    fn on_enter(&self) {
        if self.has_entered.swap(true, Ordering::Relaxed) {
            return;
        }
        // The existing runtime callback precedes finish_task_switch. Inspect
        // the actual handoff, not a task that might have exited much earlier.
        let previous = super::PREV_TASK.shared_current().lock();
        let matches = previous.as_ref().is_some_and(|task| {
            self.outgoing.ptr_eq(&crate::KtaskRef::downgrade(task))
                && task.state() == crate::TaskState::Exited
                && task.rcu_reader.is_reading()
                && task.rcu_reader.is_enrolled()
        });
        self.saw_abandoned_reader.store(matches, Ordering::Release);
    }

    fn as_any(&self) -> &dyn Any {
        self
    }
}

impl Drop for FirstEntryProbe {
    fn drop(&mut self) {
        self.is_reclaimed.store(true, Ordering::Release);
    }
}

#[def_test]
fn same_task_selection_still_records_the_reader() {
    let task = crate::current();
    let was_enrolled = task.rcu_reader.with_read(|_| {
        let rq = crate::run_queue::current_run_queue::<kspin::NoPreemptIrqSave>();
        // Model picking the already-running task using the actual same-task
        // switch path. No task state/owner is transferred to another task.
        rq.inner
            .switch_to(crate::current(), task.clone(), super::TaskState::Running);
        task.rcu_reader
            .is_enrolled_on(krcu_core::RcuContextIf::reader_node_for_tests(
                khal::percpu::this_cpu_id().as_usize(),
            ))
    });
    assert!(was_enrolled);
    assert!(!task.rcu_reader.is_enrolled());
}

#[def_test]
fn same_task_selection_cleans_deferred_reader_exit() {
    let task = crate::current();
    let (was_active, was_pending, is_clean) = {
        let _guard = kspin::NoPreemptIrqSave::new();
        let select_current_fn = |_: &krcu_core::ReadGuard<'_>| {
            let rq = crate::run_queue::current_run_queue::<kspin::NoPreemptIrqSave>();
            rq.inner
                .switch_to(crate::current(), task.clone(), super::TaskState::Running);
            task.rcu_reader.is_reading()
                && task
                    .rcu_reader
                    .is_enrolled_on(krcu_core::RcuContextIf::reader_node_for_tests(
                        khal::percpu::this_cpu_id().as_usize(),
                    ))
        };
        // SAFETY: this is the retained current task under IRQ/preemption
        // exclusion, not an interrupt fixture. Same-task selection enrolls it
        // without switching stacks. The following selection drains membership
        // before the guard allows scheduling or the task owner is released.
        let was_active = unsafe { task.rcu_reader.with_read_deferred(select_current_fn) };
        let was_pending = !task.rcu_reader.is_reading()
            && task
                .rcu_reader
                .is_enrolled_on(krcu_core::RcuContextIf::reader_node_for_tests(
                    khal::percpu::this_cpu_id().as_usize(),
                ));
        let rq = crate::run_queue::current_run_queue::<kspin::NoPreemptIrqSave>();
        rq.inner
            .switch_to(crate::current(), task.clone(), super::TaskState::Running);
        (
            was_active,
            was_pending,
            !task.rcu_reader.is_reading() && !task.rcu_reader.is_enrolled(),
        )
    };
    assert!(was_active);
    assert!(was_pending);
    assert!(is_clean);
}

#[def_test]
fn abandoned_reader_is_unlinked_and_reclaimed_after_switch() {
    let passed = Arc::new(AtomicBool::new(false));
    let result = passed.clone();
    let cpu = LogicalCpuId::new(0);
    let supervisor = crate::prepare_task(crate::TaskInner::new_pidless_kthread_with_entry(
        crate::PreparedTaskEntry::new(move || {
            let mut all_reclaimed = true;
            for _ in 0..4 {
                let runtime_dropped = Arc::new(AtomicBool::new(false));
                let abandoned = crate::prepare_task(crate::TaskInner::new_user(
                    crate::PreparedTaskEntry::new_nowait(|| {
                        let task = crate::current();
                        task.rcu_reader.with_read::<()>(|_| {
                            task.rcu_reader.with_read::<()>(|_| crate::exit(17));
                        });
                    }).expect("test entry preparation"),
                    "rcu-abandoned".into(),
                    kidentity::allocate_root_pid_handle().expect("test task identity"),
                    KBox::new(RuntimeDropProbe(runtime_dropped.clone())).unwrap(),
                ));
                abandoned.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
                crate::activate_task(&abandoned);
                let exit_code = abandoned.join();

                // Running a later task on this CPU crosses abandoned's switch
                // cleanup without assuming join waits for that cleanup.
                let sentinel = crate::prepare_task(crate::TaskInner::new_pidless_kthread_with_entry(
                    crate::PreparedTaskEntry::new_nowait(|| {}).unwrap(),
                    "rcu-switch-sentinel".into(),
                    kbuild_config::TASK_STACK_SIZE,
                ));
                sentinel.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
                let reclaimed = crate::KtaskRef::downgrade(&sentinel);
                crate::activate_task(&sentinel);
                let sentinel_status = sentinel.join();
                drop(sentinel);
                while reclaimed.strong_count() != 0 {
                    crate::yield_now();
                }

                let is_clean =
                    !abandoned.rcu_reader.is_reading() && !abandoned.rcu_reader.is_enrolled();
                let retained = crate::KtaskRef::downgrade(&abandoned);
                drop(abandoned);
                // join only publishes exit; Weak expiration proves that neither
                // the scheduler nor switch owner retains the exited task.
                while retained.strong_count() != 0 {
                    crate::yield_now();
                }
                // Weak expiry precedes asynchronous runtime destruction; wait
                // for the worker's explicit completion, not the strong count.
                while !runtime_dropped.load(Ordering::Acquire) {
                    crate::yield_now();
                }
                all_reclaimed &= exit_code == 17 && sentinel_status == 0 && is_clean;
            }
            result.store(all_reclaimed, Ordering::Release);
        }).unwrap(),
        "rcu-retirement".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    supervisor.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
    crate::activate_task(&supervisor);
    assert_eq!(supervisor.join(), 0);
    assert!(passed.load(Ordering::Acquire));
}

// Model the terminal scheduler transition with an explicitly selected successor.
// The fixture bypasses scheduling policy, not the real stack switch or either
// incoming cleanup path. Both tasks are pinned here and publication, dequeue,
// exit, and installation stay under IRQ/preemption exclusion.
fn exit_to_first_entry(receiver: crate::KtaskRef) -> ! {
    let mut rq = crate::run_queue::current_run_queue::<kspin::NoPreemptIrqSave>();
    let now_ns = khal::time::monotonic_time_nanos();
    rq.account_current_runtime(now_ns);
    let outgoing = crate::current();
    core::assert_eq!(outgoing.cpu_id(), receiver.cpu_id());
    outgoing.notify_exit(19);
    #[cfg(feature = "smp")]
    rq.inner.discharge_home(&outgoing);
    let next = rq.inner.with_sched_rank(|scheduler| {
        let next = scheduler
            .dequeue(&receiver, now_ns)
            .expect("test receiver is published on the current run queue");
        drop(scheduler.exit_current(now_ns));
        scheduler.install_current(next.clone(), now_ns);
        next
    });
    // No owning references may remain on the non-returning outgoing stack.
    drop(receiver);
    rq.inner.switch_to(outgoing, next, super::TaskState::Exited);
    unreachable!("the exited fixture cannot resume");
}

#[def_test]
fn first_task_entry_cleans_the_exited_readers_handoff() {
    let before = Arc::new(AtomicBool::new(false));
    let after = Arc::new(AtomicBool::new(false));
    let finished = Arc::new(AtomicBool::new(false));
    let before_probe = before.clone();
    let after_probe = after.clone();
    let finished_probe = finished.clone();
    let cpu = LogicalCpuId::new(0);
    let stop_work = Arc::new(kspin::SpinNoIrq::new(crate::CpuStopWork::empty()));
    let outgoing_stop_work = stop_work.clone();
    let abandoned = crate::prepare_task(crate::TaskInner::new_pidless_kthread_with_entry(
        crate::PreparedTaskEntry::new(move || {
            let task = crate::current();
            let outgoing = crate::KtaskRef::downgrade(&task);
            let outgoing_probe = outgoing.clone();
            let receiver = crate::prepare_task(crate::TaskInner::new_user(
                crate::PreparedTaskEntry::new_nowait(move || {
                    let is_clean = outgoing.upgrade().is_some_and(|old| {
                        !old.rcu_reader.is_reading() && !old.rcu_reader.is_enrolled()
                    });
                    after_probe.store(is_clean, Ordering::Release);
                }).expect("test entry preparation"),
                "rcu-first-entry".into(),
                kidentity::allocate_root_pid_handle().expect("test receiver identity"),
                KBox::new(FirstEntryProbe {
                    outgoing: outgoing_probe,
                    saw_abandoned_reader: before_probe,
                    has_entered: AtomicBool::new(false),
                    is_reclaimed: finished_probe,
                })
                .unwrap(),
            ));
            receiver.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
            let _guard = kspin::NoPreemptIrqSave::new();
            crate::activate_task(&receiver);
            // A ready Stop task outranks even maximum-priority FIFO. Keep one
            // pending to ensure this fixture does not rely on policy selection.
            // The outer test retains the stable slot until a later Stop work
            // item completes; no code moves or accesses it while queued.
            core::assert!(crate::stop_one_cpu_nowait(
                cpu,
                |_| {},
                core::ptr::null_mut(),
                &mut outgoing_stop_work.lock(),
            ));
            drop(outgoing_stop_work);
            task.rcu_reader.with_read::<()>(|_| {
                task.rcu_reader.with_read::<()>(|_| exit_to_first_entry(receiver));
            });
        }).unwrap(),
        "rcu-first-entry-outgoing".into(),
        kbuild_config::TASK_STACK_SIZE,
    ));
    abandoned.set_cpumask(crate::KCpuMask::one_shot_logical(cpu));
    crate::activate_task(&abandoned);
    let status = abandoned.join();
    // Wait for actual receiver-runtime destruction, not just its entry return.
    while !finished.load(Ordering::Acquire) {
        crate::yield_now();
    }
    // Stop work is FIFO. Completion of this later item proves that the
    // queued slot is no longer referenced, even after an assertion failure.
    let has_drained_stop_work = crate::stop_one_cpu(cpu, |_| {}, core::ptr::null_mut());
    drop(stop_work);
    assert!(has_drained_stop_work);
    assert_eq!(status, 19);
    assert!(before.load(Ordering::Acquire));
    assert!(after.load(Ordering::Acquire));
}
