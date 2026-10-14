// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Concrete Work integration without test-driven claim or manager passes.

use alloc::{boxed::Box, vec::Vec};
use core::sync::atomic::AtomicBool;

use kcpu_id_map::KCpuMaskExt;
use ktask::future::PreparedTaskWait;
use kworkerpool_ng::NormalServices;

use super::*;

fn with_normal_service<R>(
    cpu: LogicalCpuId,
    limit: NonZeroUsize,
    body: impl FnOnce(Arc<PartitionDispatch<QueuePartitionSource>>, Arc<Waker>) -> R,
) -> R {
    let services = NormalServices::start(limit, ktime_types::TimeSpan::from_millis(10)).unwrap();
    let (dispatch, kick) = services.binding(cpu).unwrap();
    let result = body(dispatch, kick);
    services.stop().unwrap();
    result
}

/// Remote execution may complete before the submitting call returns, never inline.
///
/// # Panics
/// Panics on wrong callback CPU/context, lost execution or failed producer/drain.
#[def_test(serial)]
fn remote_callback_finishes_before_submission_returns() {
    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    if cpus.len() < 2 {
        return unittest::TestResult::Ignored;
    }
    let source = cpus[0];
    let target = cpus[1];
    with_normal_service(target, NonZeroUsize::new(1).unwrap(), |dispatch, kick| {
        struct PauseReturn {
            kick: Arc<Waker>,
            done: Arc<AtomicBool>,
        }
        impl alloc::task::Wake for PauseReturn {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                self.kick.wake_by_ref();
                // Test-only post-publication pause: the real remote worker
                // must make progress while the submitter cannot run callbacks.
                while !self.done.load(Ordering::Acquire) {
                    core::hint::spin_loop();
                }
            }
        }
        let done = Arc::new(AtomicBool::new(false));
        let pause = Arc::new(Waker::from(Arc::new(PauseReturn {
            kick,
            done: done.clone(),
        })));
        let queue = queue(target, 1, &dispatch, &pause);
        let completed = done.clone();
        let work = Arc::new(Work::with_callback(move || {
            assert_eq!(khal::percpu::this_cpu_id(), target);
            assert!(ktask::is_sleepable_context());
            completed.store(true, Ordering::Release);
        }));
        let producer_queue = queue.clone();
        let producer_work = work.clone();
        let producer_done = done.clone();
        let task = ktask::prepare_task(ktask::TaskInner::new_pidless_kthread(
            move || {
                let _irq = kspin::NoPreemptIrqSave::new();
                assert_eq!(khal::percpu::this_cpu_id(), source);
                // SAFETY: the parent retains stable Arc storage and captures until
                // this producer joins and the queue finishes its final drain.
                unsafe { producer_queue.submit(&producer_work, Some(target), || unreachable!()) }
                    .unwrap();
                assert!(producer_done.load(Ordering::Acquire));
            },
            "early-work-return".into(),
            64 * 1024,
        ));
        task.set_cpumask(ktask::KCpuMask::one_shot_logical(source));
        ktask::activate_task(&task);
        assert_eq!(task.join(), 0);
        queue.close_and_drain().unwrap();
        assert!(done.load(Ordering::Acquire));
    });
}

/// An autonomous worker rejects stopping its own Work before state changes.
///
/// # Panics
/// Panics on self-wait acceptance, leaked disable ownership or failed external drain.
#[cfg(feature = "self-wait-check")]
#[def_test]
fn normal_callback_rejects_own_synchronous_stop() {
    type CallbackWork = Work<Arc<Queue>, Box<dyn Fn() + Send + Sync>>;
    let cpu = khal::percpu::this_cpu_id();
    with_normal_service(cpu, NonZeroUsize::new(1).unwrap(), |dispatch, kick| {
        let queue = queue(cpu, 1, &dispatch, &kick);
        let work: Arc<CallbackWork> = Arc::new_cyclic(|weak: &alloc::sync::Weak<CallbackWork>| {
            let weak = weak.clone();
            Work::with_callback(Box::new(move || {
                let work = weak.upgrade().unwrap();
                assert_eq!(
                    work.cancel_sync(),
                    Err(crate::work::wait::WaitError::SelfWait)
                );
                assert_eq!(work.flush(), Err(crate::work::wait::WaitError::SelfWait));
                assert!(matches!(
                    work.disable_sync(),
                    Err(crate::work::wait::WaitError::SelfWait)
                ));
                assert!(!work.state.lock().is_disabled());
            }))
        });
        // SAFETY: the Arc allocation is never moved out or mutated exclusively;
        // its owner remains alive through queue drain and all callback accesses.
        unsafe { queue.submit(&work, Some(cpu), || unreachable!()) }.unwrap();
        queue.close_and_drain().unwrap();
        assert_eq!(work.cancel_sync(), Ok(false));
    });
}

fn queue(
    cpu: LogicalCpuId,
    max_active: usize,
    dispatch: &Arc<PartitionDispatch<QueuePartitionSource>>,
    kick: &Arc<Waker>,
) -> Arc<Queue> {
    Queue::with_dispatch(
        QueueConfig::new(&[cpu], CpuPolicy::CurrentCpu, max_active).unwrap(),
        |_| (dispatch.clone(), kick.clone()),
    )
}

/// A real worker rejects its queue drain without closing admission or the source.
///
/// # Panics
/// Panics on self-wait acceptance, premature detachment or lost repeat execution.
#[cfg(feature = "self-wait-check")]
#[def_test]
fn normal_callback_rejects_own_queue_shutdown() {
    let cpu = khal::percpu::this_cpu_id();
    with_normal_service(cpu, NonZeroUsize::new(1).unwrap(), |dispatch, kick| {
        let queue = queue(cpu, 1, &dispatch, &kick);
        let receipt = queue
            .partition(cpu)
            .unwrap()
            .source_detachment_completion()
            .unwrap();
        let calls = AtomicUsize::new(0);
        let work = pin!(Work::with_callback(|| {
            assert_eq!(
                queue.close_and_drain(),
                Err(crate::work::wait::WaitError::SelfWait)
            );
            assert_eq!(queue.flush(), Err(crate::work::wait::WaitError::SelfWait));
            assert!(queue.admission_color().is_some());
            assert!(!receipt.is_completed());
            calls.fetch_add(1, Ordering::Release);
        }));
        for expected in 1..=2 {
            // SAFETY: pinned Work/captures survive callback completion and drain.
            unsafe { queue.submit(&work, Some(cpu), || unreachable!()) }.unwrap();
            while calls.load(Ordering::Acquire) != expected {
                ktask::yield_now();
            }
            work.cancel_sync().unwrap();
        }
        queue.close_and_drain().unwrap();
        assert!(receipt.is_completed());
    });
}

/// IRQ-masked publication progresses through real workers and max_active refill.
///
/// # Panics
/// Panics on failed submission/drain, wrong context/CPU, inline execution or lost work.
#[def_test]
fn normal_workers_drain_irq_masked_work_without_manual_dispatch() {
    let cpu = khal::percpu::this_cpu_id();
    let producer = ktask::current().owner_key();
    with_normal_service(cpu, NonZeroUsize::new(1).unwrap(), |dispatch, kick| {
        let queue = queue(cpu, 1, &dispatch, &kick);
        let calls = AtomicUsize::new(0);
        let works: Vec<_> = (0..13)
            .map(|_| {
                Box::pin(Work::with_callback(|| {
                    assert!(ktask::is_sleepable_context());
                    assert_eq!(khal::percpu::this_cpu_id(), cpu);
                    assert_ne!(ktask::current().owner_key(), producer);
                    calls.fetch_add(1, Ordering::Relaxed);
                }))
            })
            .collect();
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            for work in &works {
                // SAFETY: each pinned Work and its captures remain alive until
                // close_and_drain below finishes every execution and cleanup.
                unsafe { queue.submit(work, Some(cpu), || unreachable!()) }.unwrap();
            }
        }
        queue.flush().unwrap();
        for work in &works {
            work.flush().unwrap();
        }
        assert_eq!(calls.load(Ordering::Relaxed), works.len());
        assert!(queue.admission_color().is_some());
        queue.close_and_drain().unwrap();
    });
}

/// Independent work progresses during sleep only when queue allowance permits it.
///
/// # Panics
/// Panics on failed wait preparation, submission/drain or missing callback completion.
#[def_test]
fn normal_workers_grow_for_sleeping_work() {
    let cpu = khal::percpu::this_cpu_id();
    for max_active in [1, 2] {
        with_normal_service(cpu, NonZeroUsize::new(2).unwrap(), |dispatch, kick| {
            let queue = queue(cpu, max_active, &dispatch, &kick);
            let entered = AtomicBool::new(false);
            let released = AtomicBool::new(false);
            let completed = AtomicUsize::new(0);
            let wake = SpinNoIrq::new(None::<Waker>);
            let first = pin!(Work::with_callback(|| {
                let mut wait = PreparedTaskWait::prepare().unwrap();
                *wake.lock() = Some(wait.waker());
                entered.store(true, Ordering::Release);
                while !released.load(Ordering::Acquire) {
                    wait.park();
                }
                completed.fetch_add(1, Ordering::Relaxed);
            }));
            let second_entered = AtomicBool::new(false);
            let second = pin!(Work::with_callback(|| {
                second_entered.store(true, Ordering::Release);
                completed.fetch_add(1, Ordering::Relaxed);
            }));
            // SAFETY: pinned Work and all borrowed state survive the drain below.
            unsafe { queue.submit(&first, Some(cpu), || unreachable!()) }.unwrap();
            // No local time threshold: the bounded guest run diagnoses lost progress.
            while !entered.load(Ordering::Acquire) {
                ktask::yield_now();
            }
            // SAFETY: both pinned Work objects and captures survive full queue drain.
            unsafe { queue.submit(&second, Some(cpu), || unreachable!()) }.unwrap();
            if max_active == 2 {
                while !second_entered.load(Ordering::Acquire) {
                    ktask::yield_now();
                }
            } else {
                let state = second.state.lock();
                assert!(state.has_pending());
                assert!(!state.pending_is_ready());
                assert!(!second_entered.load(Ordering::Acquire));
            }
            // The test coordinator, not B, releases A after observing the required
            // outcome. Neither callback depends on another workqueue callback.
            released.store(true, Ordering::Release);
            let waker = wake.lock().as_ref().unwrap().clone();
            waker.wake();
            queue.close_and_drain().unwrap();
            assert_eq!(completed.load(Ordering::Relaxed), 2);
        });
    }
}

/// One queue's sleeping callback does not consume another queue's allowance.
///
/// # Panics
/// Panics on failed admission/drain, bypassed allowance or missing independent progress.
#[def_test]
fn sleeping_queue_preserves_independent_queue_progress_and_own_limit() {
    let cpu = khal::percpu::this_cpu_id();
    with_normal_service(cpu, NonZeroUsize::new(2).unwrap(), |dispatch, kick| {
        let first_queue = queue(cpu, 1, &dispatch, &kick);
        let second_queue = queue(cpu, 1, &dispatch, &kick);
        let entered = AtomicBool::new(false);
        let released = AtomicBool::new(false);
        let wake = SpinNoIrq::new(None::<Waker>);
        let first = pin!(Work::with_callback(|| {
            let mut wait = PreparedTaskWait::prepare().unwrap();
            *wake.lock() = Some(wait.waker());
            entered.store(true, Ordering::Release);
            while !released.load(Ordering::Acquire) {
                wait.park();
            }
        }));
        let second_entered = AtomicBool::new(false);
        let second = pin!(Work::with_callback(|| {
            second_entered.store(true, Ordering::Release);
        }));
        let third_entered = AtomicBool::new(false);
        let third = pin!(Work::with_callback(|| {
            third_entered.store(true, Ordering::Release);
        }));
        // SAFETY: this sole producer retains all pinned Work and captures
        // through both queue drains; the scoped service outlives the queues.
        unsafe { first_queue.submit(&first, Some(cpu), || unreachable!()) }.unwrap();
        while !entered.load(Ordering::Acquire) {
            ktask::yield_now();
        }
        // SAFETY: the same retained storage and final drains cover these works.
        unsafe { first_queue.submit(&third, Some(cpu), || unreachable!()) }.unwrap();
        // SAFETY: the same retained storage and final drains cover these works.
        unsafe { second_queue.submit(&second, Some(cpu), || unreachable!()) }.unwrap();
        while !second_entered.load(Ordering::Acquire) {
            ktask::yield_now();
        }
        assert!(!third_entered.load(Ordering::Acquire));
        assert!(!third.state.lock().pending_is_ready());
        released.store(true, Ordering::Release);
        let waker = wake.lock().as_ref().unwrap().clone();
        waker.wake();
        first_queue.close_and_drain().unwrap();
        second_queue.close_and_drain().unwrap();
        assert!(third_entered.load(Ordering::Acquire));
    });
}

/// Closing one queue leaves another queue's shared normal service available.
///
/// # Panics
/// Panics on failed submission/drain or missing execution on either queue.
#[def_test]
fn normal_queue_close_does_not_stop_shared_service() {
    let cpu = khal::percpu::this_cpu_id();
    with_normal_service(cpu, NonZeroUsize::new(1).unwrap(), |dispatch, kick| {
        let first_queue = queue(cpu, 1, &dispatch, &kick);
        let second_queue = queue(cpu, 1, &dispatch, &kick);
        let calls = AtomicUsize::new(0);
        let first = pin!(Work::with_callback(|| {
            calls.fetch_add(1, Ordering::Relaxed);
        }));
        let second = pin!(Work::with_callback(|| {
            calls.fetch_add(10, Ordering::Relaxed);
        }));
        // SAFETY: pinned first Work and captures outlive its queue drain.
        unsafe { first_queue.submit(&first, Some(cpu), || unreachable!()) }.unwrap();
        first_queue.close_and_drain().unwrap();
        drop(first_queue);
        // SAFETY: pinned second Work and captures outlive its queue drain.
        unsafe { second_queue.submit(&second, Some(cpu), || unreachable!()) }.unwrap();
        second_queue.close_and_drain().unwrap();
        assert_eq!(calls.load(Ordering::Relaxed), 11);
    });
}
