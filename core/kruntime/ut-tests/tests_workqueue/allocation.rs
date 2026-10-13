// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Caller-CPU heap accounting for the boot-installed NG submission paths.

use alloc::{boxed::Box, sync::Arc};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

use kalloc::current_cpu_allocated_bytes_for_test as allocated_bytes;
use ktime_types::TimeSpan;
use kworkng::{DefaultQueue, DelayedWork, Work};
use unittest::def_test;

/// Owned publication, pending cancellation and final release do not allocate locally.
///
/// # Panics
/// Panics on failed operations or caller-CPU allocation growth.
#[def_test(serial)]
fn owned_safe_submission_and_release_do_not_allocate() {
    let mut cpus = alloc::vec::Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    let config = kworkng::QueueConfig::new(&cpus, kworkng::CpuPolicy::CurrentCpu, 1).unwrap();
    let queue = kworkng::NormalQueue::new("owned-noalloc", config).unwrap();
    let handle = kworkng::WorkHandle::new(|| {}).unwrap();
    let delayed = kworkng::DelayedWorkHandle::new(|| {}).unwrap();
    {
        let irq = kspin::NoPreemptIrqSave::new();
        let before = allocated_bytes(&irq);
        for _ in 0..32 {
            handle.queue_default(None).unwrap();
            assert!(handle.cancel_pending());
            handle.queue(&queue, None).unwrap();
            assert!(handle.cancel_pending());
            delayed
                .queue(&queue, TimeSpan::from_secs(60), None)
                .unwrap();
            delayed.modify(&queue, TimeSpan::ZERO, None).unwrap();
            delayed
                .modify(&queue, TimeSpan::from_secs(30), None)
                .unwrap();
            assert!(delayed.cancel_pending());
            delayed
                .queue_default(TimeSpan::from_secs(60), None)
                .unwrap();
            delayed.modify_default(TimeSpan::ZERO, None).unwrap();
            assert!(delayed.cancel_pending());
        }
        drop(handle);
        drop(delayed);
        assert_eq!(allocated_bytes(&irq), before);
    }
    queue.close_and_drain().unwrap();
}

/// Real hard IRQ can publish and release owned ordinary/delayed work without allocation.
///
/// # Panics
/// Panics on wrong IRQ/destructor context, failed admission, allocation or lost cleanup.
#[def_test(serial)]
fn real_hardirq_releases_owned_work_without_allocation() {
    struct Capture(Arc<AtomicUsize>);
    impl Capture {
        fn check(&self) {
            assert!(ktask::is_sleepable_context());
        }
    }
    impl Drop for Capture {
        fn drop(&mut self) {
            assert!(ktask::is_sleepable_context());
            self.0.fetch_add(1, Ordering::Release);
        }
    }
    for remote in [false, true] {
        if remote && kcpu_id_map::nr_cpus() < 2 {
            continue;
        }
        let drops = Arc::new(AtomicUsize::new(0));
        let capture = Capture(drops.clone());
        let work = kworkng::WorkHandle::new(move || capture.check()).unwrap();
        let capture = Capture(drops.clone());
        let delayed = kworkng::DelayedWorkHandle::new(move || capture.check()).unwrap();
        let handles = kspin::SpinNoIrq::new(Some((work, delayed)));
        let done = Arc::new(AtomicBool::new(false));
        let producer_done = done.clone();
        let producer =
            ktask::Timer::new(move |_: ktask::TimerControl<'_>, _: ktask::Expiration| {
                assert!(kirq::context::is_in_hardirq());
                let irq = kspin::NoPreemptIrqSave::new();
                let before = allocated_bytes(&irq);
                let source = khal::percpu::this_cpu_id();
                let mut target = source;
                if remote {
                    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
                        if cpu != source {
                            target = cpu;
                        }
                    });
                    assert_ne!(target, source);
                }
                let (work, delayed) = handles.lock().take().unwrap();
                work.queue_default(Some(target)).unwrap();
                delayed
                    .queue_default(TimeSpan::from_secs(60), Some(target))
                    .unwrap();
                delayed
                    .modify_default(TimeSpan::from_secs(30), Some(target))
                    .unwrap();
                assert!(delayed.cancel_pending());
                delayed
                    .queue_default(TimeSpan::from_nanos(1), Some(target))
                    .unwrap();
                drop(work);
                drop(delayed);
                assert_eq!(allocated_bytes(&irq), before);
                producer_done.store(true, Ordering::Release);
            });
        // SAFETY: boot timer services are ready, the bounded IRQ callback owns
        // its handles, and the parent drains this producer before releasing it.
        unsafe { producer.start(khal::time::monotonic_time() + TimeSpan::from_millis(1)) }.unwrap();
        let limit = khal::time::monotonic_time() + TimeSpan::from_secs(5);
        while (!done.load(Ordering::Acquire) || drops.load(Ordering::Acquire) != 2)
            && khal::time::monotonic_time() < limit
        {
            ktask::sleep(TimeSpan::from_millis(1));
        }
        producer.cancel_sync();
        assert!(done.load(Ordering::Acquire));
        assert_eq!(drops.load(Ordering::Acquire), 2);
    }
}

/// Safe static publication reuses storage without caller-CPU allocation.
/// The Work intentionally remains allocated for the guest lifetime.
///
/// # Panics
/// Panics on admission/cancel failure, allocation growth or failed final drain.
#[def_test(serial)]
fn static_safe_submission_does_not_allocate() {
    let mut cpus = alloc::vec::Vec::new();
    kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| cpus.push(cpu));
    let config = kworkng::QueueConfig::new(&cpus, kworkng::CpuPolicy::CurrentCpu, 1).unwrap();
    let queue = kworkng::NormalQueue::new("static-noalloc", config).unwrap();
    let work: &'static _ = Box::leak(Box::new(Work::with_callback(|| {})));
    for _ in 0..16 {
        {
            let irq = kspin::NoPreemptIrqSave::new();
            let before = allocated_bytes(&irq);
            kworkng::queue_static_work(work, None).unwrap();
            assert!(work.cancel_pending());
            queue.queue_static_work(work, None).unwrap();
            assert!(work.cancel_pending());
            assert_eq!(allocated_bytes(&irq), before);
        }
        work.cancel_sync().unwrap();
    }
    queue.close_and_drain().unwrap();
}

/// Prepared ordinary and delayed operations do not allocate on the submitting CPU.
///
/// # Panics
/// Panics on a broken allocation probe, admission/drain failure or any measured
/// heap allocation. IRQ-masked task context is not actual hard-IRQ entry; remote
/// receiver execution and sleepable synchronous drain are outside this probe.
#[def_test(serial)]
fn prepared_ng_operations_do_not_allocate_on_caller_cpu() {
    {
        let irq = kspin::NoPreemptIrqSave::new();
        let before = allocated_bytes(&irq);
        let allocation = core::hint::black_box(Box::new([0u8; 64]));
        drop(allocation);
        assert!(allocated_bytes(&irq) >= before + 64);
    }
    for queue in [
        DefaultQueue::Normal,
        DefaultQueue::BottomHalf,
        DefaultQueue::HighBottomHalf,
    ] {
        kcpu_id_map::for_each_present_logical_cpu(|_, target, _| {
            let work = Box::pin(Work::with_callback(|| {}));
            let delayed = Box::pin(DelayedWork::new(|| {}));
            {
                let irq = kspin::NoPreemptIrqSave::new();
                let before = allocated_bytes(&irq);
                for _ in 0..16 {
                    // SAFETY: pinned storage remains owned here through final
                    // synchronous drain; no other producer retains this Work.
                    unsafe { queue.queue_work(work.as_ref(), Some(target)) }.unwrap();
                    assert_eq!(
                        allocated_bytes(&irq),
                        before,
                        "ordinary submit {queue:?} {target:?}"
                    );
                    work.cancel_pending();
                    assert_eq!(
                        allocated_bytes(&irq),
                        before,
                        "ordinary cancel {queue:?} {target:?}"
                    );
                }
                assert_eq!(allocated_bytes(&irq), before, "ordinary admission/cancel");
            }
            work.cancel_sync().unwrap();
            {
                let irq = kspin::NoPreemptIrqSave::new();
                let before = allocated_bytes(&irq);
                for _ in 0..16 {
                    // SAFETY: the pinned pair/captures remain alive until both
                    // physical timer and Work are drained below, with no peers.
                    unsafe {
                        queue.queue_delayed_work(
                            delayed.as_ref(),
                            TimeSpan::from_secs(60),
                            Some(target),
                        )
                    }
                    .unwrap();
                    // SAFETY: same retained pair and final drain as admission.
                    unsafe {
                        queue.modify_delayed_work(
                            delayed.as_ref(),
                            TimeSpan::from_secs(30),
                            Some(target),
                        )
                    }
                    .unwrap();
                    delayed.cancel_pending();
                }
                assert_eq!(allocated_bytes(&irq), before, "delayed arm/modify/cancel");
            }
            delayed.cancel_sync().unwrap();
        });
    }
}

/// Real timer IRQ submission stays allocation-free and reaches local/remote workers.
///
/// # Panics
/// Panics if real IRQ delivery or work completion times out, the execution CPU
/// or context is wrong, an API fails, or submitting-CPU heap bytes increase.
#[def_test(serial)]
fn real_hardirq_submits_prepared_ng_work_without_allocation() {
    for queue in [
        DefaultQueue::Normal,
        DefaultQueue::BottomHalf,
        DefaultQueue::HighBottomHalf,
    ] {
        for remote in [false, true] {
            let pin = kspin::NoPreempt::new();
            let source = khal::percpu::this_cpu_id();
            let mut target = source;
            if remote {
                kcpu_id_map::for_each_present_logical_cpu(|_, cpu, _| {
                    if cpu != source {
                        target = cpu;
                    }
                });
                if target == source {
                    continue;
                }
            }
            let is_valid = Arc::new(AtomicBool::new(true));
            let producer_done = Arc::new(AtomicBool::new(false));
            let ordinary_calls = Arc::new(AtomicUsize::new(0));
            let delayed_calls = Arc::new(AtomicUsize::new(0));
            let callback = |calls: Arc<AtomicUsize>| {
                let is_valid = is_valid.clone();
                move || {
                    if khal::percpu::this_cpu_id() != target || kirq::context::is_in_hardirq() {
                        is_valid.store(false, Ordering::Release);
                    }
                    calls.fetch_add(1, Ordering::Release);
                }
            };
            let work = Arc::pin(Work::with_callback(callback(ordinary_calls.clone())));
            let delayed = Arc::pin(DelayedWork::new(callback(delayed_calls.clone())));
            let producer_work = work.clone();
            let producer_delayed = delayed.clone();
            let producer_valid = is_valid.clone();
            let done = producer_done.clone();
            let producer =
                ktask::Timer::new(move |_: ktask::TimerControl<'_>, _: ktask::Expiration| {
                    if !kirq::context::is_in_hardirq()
                        || karch::local_irq_enabled()
                        || khal::percpu::this_cpu_id() != source
                    {
                        producer_valid.store(false, Ordering::Release);
                    }
                    let irq = kspin::NoPreemptIrqSave::new();
                    let before = allocated_bytes(&irq);
                    // SAFETY: the parent retains pinned Work/captures through timer
                    // cancellation (producer join) and final synchronous Work drain.
                    let ordinary =
                        unsafe { queue.queue_work(producer_work.as_ref(), Some(target)) };
                    // SAFETY: the same parent retains the pinned timer/Work pair
                    // through producer quiescence and both physical/logical drains.
                    let armed = unsafe {
                        queue.queue_delayed_work(
                            producer_delayed.as_ref(),
                            TimeSpan::from_secs(60),
                            Some(target),
                        )
                    };
                    // SAFETY: same retained pair, sole producer and final drain.
                    let modified = unsafe {
                        queue.modify_delayed_work(
                            producer_delayed.as_ref(),
                            TimeSpan::from_secs(30),
                            Some(target),
                        )
                    };
                    let canceled = producer_delayed.cancel_pending();
                    // SAFETY: reuse of the same retained pair; eventual delivery
                    // does not authorize releasing storage before the final drain.
                    let delivered = unsafe {
                        queue.queue_delayed_work(
                            producer_delayed.as_ref(),
                            TimeSpan::from_millis(1),
                            Some(target),
                        )
                    };
                    if ordinary.is_err()
                        || armed.is_err()
                        || modified.is_err()
                        || !canceled
                        || delivered.is_err()
                        || allocated_bytes(&irq) != before
                    {
                        producer_valid.store(false, Ordering::Release);
                    }
                    done.store(true, Ordering::Release);
                });
            // SAFETY: boot timer service is ready; the source CPU is pinned.
            // The handler is bounded/nonblocking and owns all captured storage.
            let started =
                unsafe { producer.start(khal::time::monotonic_time() + TimeSpan::from_millis(2)) };
            drop(pin);
            let limit = khal::time::monotonic_time() + TimeSpan::from_secs(3);
            while (!producer_done.load(Ordering::Acquire)
                || ordinary_calls.load(Ordering::Acquire) == 0
                || delayed_calls.load(Ordering::Acquire) == 0)
                && khal::time::monotonic_time() < limit
            {
                ktask::sleep(TimeSpan::from_millis(1));
            }
            // Join the IRQ producer before draining any storage it could submit.
            producer.cancel_sync();
            work.cancel_sync().unwrap();
            delayed.cancel_sync().unwrap();
            assert!(started.is_ok());
            assert!(producer_done.load(Ordering::Acquire));
            assert!(
                is_valid.load(Ordering::Acquire),
                "{queue:?}, remote={remote}"
            );
            assert_eq!(ordinary_calls.load(Ordering::Acquire), 1);
            assert_eq!(delayed_calls.load(Ordering::Acquire), 1);
        }
    }
}
