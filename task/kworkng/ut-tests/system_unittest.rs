// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Live boot-installed services; never replace softirq actions or the registry.

use core::{
    pin::pin,
    sync::atomic::{AtomicUsize, Ordering},
};

use ktime_types::TimeSpan;
use unittest::def_test;

use crate::{DefaultQueue, DelayedWork, Work};

/// Safe static entry points reuse storage across CPUs and reject closed queues.
/// Storage intentionally remains allocated for the guest's lifetime, as required
/// by these APIs; producers stop and drain before this test returns.
///
/// # Panics
/// Panics on admission/drain failure, wrong callback context/CPU, lost callbacks
/// or acceptance after queue closure.
#[def_test(serial)]
fn static_work_uses_normal_execution_and_shared_closure() {
    use alloc::{boxed::Box, sync::Arc, vec::Vec};

    let mut cpus = Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    let config = crate::QueueConfig::new(&cpus, crate::CpuPolicy::CurrentCpu, 1).unwrap();
    let queue = crate::NormalQueue::new("static-work-test", config).unwrap();
    let other = queue.clone();
    let calls = Arc::new(AtomicUsize::new(0));
    let expected_cpu = Arc::new(AtomicUsize::new(0));
    let callback_calls = calls.clone();
    let callback_cpu = expected_cpu.clone();
    let work: &'static _ = Box::leak(Box::new(Work::with_callback(move || {
        assert!(ktask::is_sleepable_context());
        assert_eq!(
            khal::percpu::this_cpu_id().as_usize(),
            callback_cpu.load(Ordering::Acquire)
        );
        callback_calls.fetch_add(1, Ordering::Release);
    })));
    let mut expected = 0;
    for cpu in cpus {
        expected_cpu.store(cpu.as_usize(), Ordering::Release);
        for _ in 0..4 {
            {
                let _irq = kspin::NoPreemptIrqSave::new();
                crate::queue_static_work(work, Some(cpu)).unwrap();
            }
            work.flush().unwrap();
            work.cancel_sync().unwrap();
            expected += 1;
            assert_eq!(calls.load(Ordering::Acquire), expected);
            {
                let _irq = kspin::NoPreemptIrqSave::new();
                other.queue_static_work(work, Some(cpu)).unwrap();
            }
            queue.flush().unwrap();
            work.cancel_sync().unwrap();
            expected += 1;
            assert_eq!(calls.load(Ordering::Acquire), expected);
        }
    }
    queue.close_and_drain().unwrap();
    assert_eq!(
        other.queue_static_work(work, None),
        Err(crate::SubmitError::Closed)
    );
    assert_eq!(calls.load(Ordering::Acquire), expected);
}

/// All boot CPUs execute reused Work through each boot-installed category.
///
/// # Panics
/// Panics on failed submission/drain, wrong CPU or lost/duplicate callbacks.
#[def_test(serial)]
fn boot_services_execute_reused_work_on_all_cpus() {
    for queue in [
        DefaultQueue::Normal,
        DefaultQueue::BottomHalf,
        DefaultQueue::HighBottomHalf,
    ] {
        kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
            let calls = AtomicUsize::new(0);
            let work = pin!(Work::with_callback(|| {
                assert_eq!(khal::percpu::this_cpu_id(), cpu);
                calls.fetch_add(1, Ordering::Release);
            }));
            for _ in 0..2 {
                {
                    let _irq = kspin::NoPreemptIrqSave::new();
                    // SAFETY: pinned local storage/captures live through flush
                    // and final cancel; no other producer retains this Work.
                    unsafe { queue.queue_work(work.as_ref(), Some(cpu)) }.unwrap();
                }
                work.flush().unwrap();
            }
            work.cancel_sync().unwrap();
            assert_eq!(calls.load(Ordering::Acquire), 2);
        });
    }
}

/// Boot timer delivery transfers prepared delayed work to every execution kind.
///
/// # Panics
/// Panics on failed timer submission, missing delivery or incomplete drain.
#[def_test(serial)]
fn boot_services_deliver_and_drain_delayed_work() {
    for queue in [
        DefaultQueue::Normal,
        DefaultQueue::BottomHalf,
        DefaultQueue::HighBottomHalf,
    ] {
        let calls = AtomicUsize::new(0);
        let work = pin!(DelayedWork::new(|| {
            calls.fetch_add(1, Ordering::Release);
        }));
        // SAFETY: there is one producer; pinned pair and captures remain alive
        // through physical timer and Work drain, with callbacks able to progress.
        unsafe { queue.queue_delayed_work(work.as_ref(), TimeSpan::from_millis(2), None) }.unwrap();
        let limit = khal::time::monotonic_time() + TimeSpan::from_secs(2);
        while calls.load(Ordering::Acquire) == 0 && khal::time::monotonic_time() < limit {
            ktask::sleep(TimeSpan::from_millis(1));
        }
        work.cancel_sync().unwrap();
        assert_eq!(calls.load(Ordering::Acquire), 1);
    }
}
