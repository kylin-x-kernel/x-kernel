// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::PollState;

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
