// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kalloc_types::KBox;
use unittest::def_test;

use super::{CpuPolicy, NetworkPoller, NormalQueue, PollState, QueueConfig};

/// Foreground demand remains pending until queue startup can replay it.
///
/// # Panics
/// Panics if unprepared polling consumes demand or claims execution.
#[def_test]
fn foreground_notification_before_start_preserves_demand() {
    let poller = KBox::leak(KBox::new(NetworkPoller::new()).unwrap());

    poller.notify_and_assist(super::PollReason::Tx);
    let state = poller.state.lock();
    assert!(state.queue.is_none());
    assert!(state.has_pending);
    assert!(!state.is_running);
}

/// A foreground notification cannot displace the active executor or lose demand.
///
/// # Panics
/// Panics if queue preparation fails or pending demand does not survive ownership.
#[def_test]
fn foreground_notification_preserves_running_owner() {
    let config =
        QueueConfig::new(&[khal::percpu::this_cpu_id()], CpuPolicy::CurrentCpu, 1).unwrap();
    let queue = NormalQueue::new("knet-poller-test", config).unwrap();
    let poller = KBox::leak(KBox::new(NetworkPoller::new()).unwrap());
    {
        let mut state = poller.state.lock();
        state.queue = Some(queue.clone());
        state.is_running = true;
    }

    poller.notify_and_assist(super::PollReason::Tx);
    {
        let mut state = poller.state.lock();
        assert!(state.is_running);
        assert!(state.has_pending);
        assert!(state.finish(false));
    }
    assert!(poller.try_claim(false));
    {
        let mut state = poller.state.lock();
        assert!(!state.has_pending);
        assert!(!state.finish(false));
    }
    let _ = poller.state.lock().queue.take();
    queue.close_and_drain().unwrap();
}

/// Fully consumed foreground demand does not submit an empty background round.
///
/// # Panics
/// Panics if foreground polling is skipped, exceeds its budget, or queues work.
#[def_test]
fn foreground_round_does_not_queue_completed_work() {
    use core::sync::atomic::{AtomicUsize, Ordering};

    use ktime_types::TimeSpan;
    use kworkng::QueueFreezeCancellation;

    static POLL_ROUNDS: AtomicUsize = AtomicUsize::new(0);

    let config =
        QueueConfig::new(&[khal::percpu::this_cpu_id()], CpuPolicy::CurrentCpu, 1).unwrap();
    let queue = NormalQueue::new("knet-foreground-test", config).unwrap();
    // Retain any premature submission so a worker cannot hide it by running first.
    let frozen = queue
        .freeze(TimeSpan::from_secs(2), &QueueFreezeCancellation::new())
        .unwrap();
    let mut poller = NetworkPoller::new();
    // Keep this poller's progress independent of live network traffic.
    poller.poll_once = |budget| {
        assert_eq!(budget.rx_packets, super::ASSIST_BUDGET.rx_packets);
        assert_eq!(budget.tx_packets, super::ASSIST_BUDGET.tx_packets);
        assert_eq!(budget.timer_events, super::ASSIST_BUDGET.timer_events);
        POLL_ROUNDS.fetch_add(1, Ordering::Relaxed);
        super::super::PollProgress::default()
    };
    poller.state.lock().queue = Some(queue.clone());
    let poller = KBox::leak(KBox::new(poller).unwrap());
    POLL_ROUNDS.store(0, Ordering::Relaxed);

    poller.notify_and_assist(super::PollReason::Tx);

    let was_queued = poller.work.cancel_pending();
    let _ = poller.state.lock().queue.take();
    drop(frozen);
    queue.close_and_drain().unwrap();
    assert_eq!(POLL_ROUNDS.load(Ordering::Relaxed), 1);
    assert!(!was_queued);
    let state = poller.state.lock();
    assert!(!state.is_running);
    assert!(!state.has_pending);
}

/// IRQ-side requests reach the boot-bound reusable network Work.
///
/// # Panics
/// Panics if startup omitted the private queue or notifications make no progress.
#[def_test(serial)]
fn boot_network_poller_handles_irq_notifications() {
    use core::sync::atomic::Ordering;

    use ktime_types::TimeSpan;

    let poller = super::super::network_poller();
    assert!(poller.state.lock().queue.is_some());
    for _ in 0..2 {
        let before = super::BACKGROUND_RUNS.load(Ordering::Acquire);
        {
            let _irq = kspin::NoPreemptIrqSave::new();
            for _ in 0..16 {
                poller.notify(super::PollReason::Rx);
            }
        }
        let limit = khal::time::monotonic_time() + TimeSpan::from_secs(2);
        while super::BACKGROUND_RUNS.load(Ordering::Acquire) == before
            && khal::time::monotonic_time() < limit
        {
            ktask::sleep(TimeSpan::from_millis(1));
        }
        assert!(super::BACKGROUND_RUNS.load(Ordering::Acquire) > before);
    }
    // Do not cancel the kernel-lifetime poller: network producers stay live.
}

/// Concurrent demand survives completion; a second executor cannot claim.
///
/// # Panics
/// Panics if running demand is lost or two executors can own the poller.
#[def_test]
fn notifications_during_poll_require_another_round() {
    let mut state = PollState {
        queue: None,
        is_running: false,
        has_pending: true,
    };
    assert!(state.claim());
    state.has_pending = true;
    assert!(!state.claim());
    assert!(state.finish(false));
    assert!(state.claim());
    assert!(!state.finish(false));
    assert!(!state.claim());
}

/// Exhausted budget preserves demand even without another IRQ notification.
///
/// # Panics
/// Panics if budget exhaustion cannot schedule a continuation.
#[def_test]
fn budget_exhaustion_preserves_pending_demand() {
    let mut state = PollState {
        queue: None,
        is_running: false,
        has_pending: true,
    };
    assert!(state.claim());
    assert!(state.finish(true));
    assert!(state.claim());
    assert!(!state.finish(false));
    state.has_pending = true;
    assert!(state.claim());
}
