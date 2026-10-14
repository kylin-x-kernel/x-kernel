// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kirq::{context, softirq::test_support::ScopedDaemonWakeGate};
use unittest::{assert, assert_eq, def_test};

use super::*;
use crate::block_completion::prepare_block_wait;

struct Action<F>(F);
impl<F: Fn() + Send + Sync> BlockCompletionOperations for Action<F> {
    fn process_completed_requests(&self) {
        (self.0)();
    }
}

fn device(action: impl Fn() + Send + Sync + 'static) -> Arc<dyn BlockCompletionOperations> {
    Arc::new(Action(action))
}

#[def_test(serial)]
fn block_completion_dispatch_coalesces_and_requeues_only_into_next_batch() {
    let _wake_gate = ScopedDaemonWakeGate::disabled();
    let calls = Arc::new(AtomicUsize::new(0));
    let reclaimer = Arc::new(SpinNoIrq::new(None::<Arc<BlockIoReclaimer>>));
    let device = device({
        let calls = calls.clone();
        let reclaimer = reclaimer.clone();
        move || {
            if calls.fetch_add(1, Ordering::Relaxed) == 0 {
                reclaimer.lock().as_ref().unwrap().mark_pending();
                reclaimer.lock().as_ref().unwrap().mark_pending();
            }
        }
    });
    let registration = BlockIoReclaimer::new(Arc::downgrade(&device));
    *reclaimer.lock() = Some(registration.clone());
    let mut waiter = prepare_block_wait().unwrap();
    {
        let _bh = context::local_bh_disable();
        registration.clone().mark_pending();
        registration.clone().mark_pending();
        process_pending_devices();
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        process_pending_devices();
        assert_eq!(calls.load(Ordering::Relaxed), 2);
        softirq::clear_softirq_pending_for_tests(SoftirqVec::Block);
    }
    registration.stop_and_wait(waiter.as_mut());
    let mut waiter = prepare_block_wait().unwrap();
    registration.stop_and_wait(waiter.as_mut());
    registration.clone().mark_pending();
    process_pending_devices();
    assert_eq!(calls.load(Ordering::Relaxed), 2);
}

#[def_test(serial)]
fn block_completion_dispatch_disabled_detached_and_expired_targets_retire() {
    let _wake_gate = ScopedDaemonWakeGate::disabled();
    let calls = Arc::new(AtomicUsize::new(0));
    let second_target = device({
        let calls = calls.clone();
        move || {
            calls.fetch_add(1, Ordering::Relaxed);
        }
    });
    let second = BlockIoReclaimer::new(Arc::downgrade(&second_target));
    let second_handle = second.clone();
    let owner = Arc::new(SpinNoIrq::new(Some(second)));
    let first_target = device({
        let owner = owner.clone();
        let reclaimer = second_handle.clone();
        move || {
            reclaimer.mark_pending();
            // Simulate stop admission while this CPU owns a detached batch.
            // The task-context wait itself is covered by the remote-stop test.
            let closing = owner.lock().take().unwrap();
            update_state(&closing, |state, _| state.is_accepting = false);
        }
    });
    let first = BlockIoReclaimer::new(Arc::downgrade(&first_target));
    let expired_target = device(|| panic!("expired device ran"));
    let expired = BlockIoReclaimer::new(Arc::downgrade(&expired_target));
    drop(expired_target);
    {
        let _bh = context::local_bh_disable();
        first.clone().mark_pending();
        second_handle.mark_pending();
        expired.clone().mark_pending();
        process_pending_devices();
        process_pending_devices();
        softirq::clear_softirq_pending_for_tests(SoftirqVec::Block);
    }
    assert_eq!(calls.load(Ordering::Relaxed), 0);
    assert_eq!(Arc::strong_count(&second_handle), 2);
    assert_eq!(Arc::strong_count(&expired), 1);
}

fn spawn_on(cpu: usize, action: impl FnOnce() + Send + 'static) -> ktask::KtaskRef {
    let task = ktask::TaskInner::new_kthread(action, "block-registration-test".into(), 0x8000)
        .expect("test kthread allocation must succeed");
    let mut mask = ktask::KCpuMask::new();
    mask.set(cpu, true);
    task.set_cpumask(mask);
    ktask::spawn_task(task)
}

struct Affinity(ktask::KCpuMask);
impl Drop for Affinity {
    fn drop(&mut self) {
        core::assert!(ktask::set_current_affinity(self.0));
    }
}

#[def_test(serial)]
fn block_completion_dispatch_remote_schedule_and_stop_wait_for_callback_retirement() {
    if kcpu_id_map::nr_cpus() < 2 {
        return unittest::TestResult::Ignored;
    }
    let _affinity = Affinity(ktask::current().cpumask());
    let local = {
        let _pin = NoPreempt::new();
        let cpu = khal::percpu::this_cpu_id().as_usize();
        let mut mask = ktask::KCpuMask::new();
        mask.set(cpu, true);
        assert!(ktask::set_current_affinity(mask));
        cpu
    };
    let remote = if local == 0 { 1 } else { 0 };
    for stop_running in [false, true] {
        let queued = Arc::new(AtomicBool::new(false));
        let consume = Arc::new(AtomicBool::new(false));
        let entered = Arc::new(AtomicBool::new(false));
        let release = Arc::new(AtomicBool::new(false));
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_cpu = Arc::new(AtomicUsize::new(usize::MAX));
        let device = device({
            let entered = entered.clone();
            let release = release.clone();
            let calls = calls.clone();
            let callback_cpu = callback_cpu.clone();
            move || {
                calls.fetch_add(1, Ordering::Relaxed);
                callback_cpu.store(khal::percpu::this_cpu_id().as_usize(), Ordering::Relaxed);
                entered.store(true, Ordering::Release);
                while !release.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
            }
        });
        let registration = BlockIoReclaimer::new(Arc::downgrade(&device));
        let reclaimer = registration.clone();
        let producer = spawn_on(remote, {
            let reclaimer = reclaimer.clone();
            let queued = queued.clone();
            let consume = consume.clone();
            move || {
                {
                    let _irq = kspin::NoPreemptIrqSave::new();
                    reclaimer.mark_pending();
                    queued.store(true, Ordering::Release);
                    while !consume.load(Ordering::Acquire) {
                        core::hint::spin_loop();
                    }
                }
                softirq::run_pending_softirqs();
            }
        });
        while !queued.load(Ordering::Acquire) {
            ktask::yield_now();
        }
        reclaimer.mark_pending(); // Coalesce on the remote queued owner, never local.
        if stop_running {
            consume.store(true, Ordering::Release);
            while !entered.load(Ordering::Acquire) {
                ktask::yield_now();
            }
            reclaimer.mark_pending(); // RunningAgain must be suppressed by stop.
        }
        let closer = spawn_on(local, {
            let registration = registration.clone();
            move || {
                let mut waiter = prepare_block_wait().unwrap();
                registration.stop_and_wait(waiter.as_mut());
            }
        });
        while !matches!(
            closer.state(),
            ktask::TaskState::Blocked | ktask::TaskState::Exited
        ) {
            ktask::yield_now();
        }
        let second_closer = spawn_on(local, move || {
            let mut waiter = prepare_block_wait().unwrap();
            registration.stop_and_wait(waiter.as_mut());
        });
        while !matches!(
            second_closer.state(),
            ktask::TaskState::Blocked | ktask::TaskState::Exited
        ) {
            ktask::yield_now();
        }
        let returned_early = closer.state() == ktask::TaskState::Exited
            || second_closer.state() == ktask::TaskState::Exited;
        reclaimer.mark_pending(); // Disabled, whether queued or running.
        release.store(true, Ordering::Release);
        consume.store(true, Ordering::Release);
        assert_eq!(closer.join(), 0);
        assert_eq!(second_closer.join(), 0);
        assert_eq!(producer.join(), 0);
        assert!(!returned_early);
        assert_eq!(calls.load(Ordering::Relaxed), usize::from(stop_running));
        assert_eq!(
            callback_cpu.load(Ordering::Relaxed),
            if stop_running { remote } else { usize::MAX }
        );
        assert_eq!(Arc::strong_count(&device), 1);
    }
}
