// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Payload publication through real boot-installed workqueue synchronization.
//! Providers intentionally remain installed for the full guest lifetime.

use alloc::boxed::Box;
use core::sync::atomic::{AtomicUsize, Ordering};

use ktime_types::TimeSpan;
use kworkng::{DefaultQueue, DelayedWork, Work};
use unittest::def_test;

/// Fresh submission publishes writes and flush makes callback writes observable.
/// Payload atomics deliberately provide no independent release/acquire handoff.
///
/// # Panics
/// Panics on admission/drain failure, stale payload, incorrect CPU or lost remote coverage.
#[def_test(serial)]
fn fresh_submission_and_flush_publish_relaxed_payloads() {
    let mut remote_submissions = 0;
    for queue in [
        DefaultQueue::Normal,
        DefaultQueue::BottomHalf,
        DefaultQueue::HighBottomHalf,
    ] {
        kcpu_id_map::for_each_present_logical_cpu(|_, target, _| {
            let input = AtomicUsize::new(0);
            let output = AtomicUsize::new(0);
            let callback = || {
                assert_eq!(khal::percpu::this_cpu_id(), target);
                output.store(input.load(Ordering::Relaxed) * 3, Ordering::Relaxed);
            };
            let ordinary = Box::pin(Work::with_callback(callback));
            let delayed = Box::pin(DelayedWork::new(callback));
            for round in 1..=128 {
                input.store(round, Ordering::Relaxed);
                output.store(0, Ordering::Relaxed);
                {
                    let _irq = kspin::NoPreemptIrqSave::new();
                    remote_submissions += usize::from(khal::percpu::this_cpu_id() != target);
                    // SAFETY: sole producer retains pinned Work/captures until
                    // every flush and final drain; no storage is moved or freed.
                    unsafe { queue.queue_work(ordinary.as_ref(), Some(target)) }.unwrap();
                }
                ordinary.flush().unwrap();
                assert_eq!(output.load(Ordering::Relaxed), round * 3);
                input.store(round + 1000, Ordering::Relaxed);
                output.store(0, Ordering::Relaxed);
                // SAFETY: the same retained lifetime covers the prepared timer
                // and Work pair through physical cancellation below.
                unsafe {
                    queue.queue_delayed_work(
                        delayed.as_ref(),
                        TimeSpan::from_nanos(1),
                        Some(target),
                    )
                }
                .unwrap();
                delayed.flush().unwrap();
                delayed.cancel_sync().unwrap();
                assert_eq!(output.load(Ordering::Relaxed), (round + 1000) * 3);
            }
            ordinary.cancel_sync().unwrap();
            delayed.cancel_sync().unwrap();
        });
    }
    if kcpu_id_map::nr_cpus() > 1 {
        assert!(remote_submissions > 0);
    }
}
