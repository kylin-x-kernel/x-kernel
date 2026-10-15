// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::{NotificationFlags, PendingWord};

#[def_test]
fn fixed_early_request_survives_unrelated_ipi() {
    let pending = PendingWord::new();
    assert!(!pending.request(NotificationFlags::RESCHEDULE));
    assert!(pending.take().is_empty());
    pending.enable();
    assert_eq!(pending.take(), NotificationFlags::RESCHEDULE);
    assert!(pending.take().is_empty());
}

#[def_test]
fn fixed_request_between_enable_and_drain() {
    let pending = PendingWord::new();
    pending.enable();
    assert!(pending.request(NotificationFlags::RESCHEDULE));
    assert_eq!(pending.take(), NotificationFlags::RESCHEDULE);
    assert!(pending.take().is_empty());
}

#[def_test]
fn fixed_requests_coalesce_without_losing_later_request() {
    let pending = PendingWord::new();
    pending.enable();
    assert!(pending.take().is_empty());
    assert!(pending.request(NotificationFlags::RESCHEDULE));
    assert!(pending.request(NotificationFlags::RESCHEDULE));
    assert_eq!(pending.take(), NotificationFlags::RESCHEDULE);
    assert!(pending.request(NotificationFlags::RESCHEDULE));
    assert_eq!(pending.take(), NotificationFlags::RESCHEDULE);
    assert!(pending.take().is_empty());
}

#[def_test]
fn timer_and_reschedule_survive_boot_drain_together() {
    let pending = PendingWord::new();
    assert!(!pending.request(NotificationFlags::TIMER_REPROGRAM));
    assert!(!pending.request(NotificationFlags::RESCHEDULE));
    assert!(pending.take().is_empty());
    pending.enable();
    assert_eq!(
        pending.take(),
        NotificationFlags::TIMER_REPROGRAM | NotificationFlags::RESCHEDULE
    );
    assert!(pending.take().is_empty());
}

#[def_test]
fn timer_requests_coalesce_and_preserve_requests_during_service() {
    let pending = PendingWord::new();
    pending.enable();
    assert!(pending.request(NotificationFlags::TIMER_REPROGRAM));
    assert!(pending.request(NotificationFlags::TIMER_REPROGRAM));
    assert_eq!(pending.take(), NotificationFlags::TIMER_REPROGRAM);
    // A sender after the drain must leave work for the next IPI, including
    // when the receiver is still programming hardware for the old request.
    assert!(pending.request(NotificationFlags::TIMER_REPROGRAM));
    assert!(pending.request(NotificationFlags::RESCHEDULE));
    assert_eq!(
        pending.take(),
        NotificationFlags::TIMER_REPROGRAM | NotificationFlags::RESCHEDULE
    );
    assert!(pending.take().is_empty());
}

#[cfg(feature = "timer-reprogram")]
#[def_test]
fn timer_notification_rejects_out_of_range_cpu() {
    assert_eq!(
        super::notify_timer_reprogram(kbuild_config::NR_CPUS.into()),
        Err(crate::KipiError::InvalidCpuId)
    );
}

/// All fixed reasons survive boot enablement without overwriting each other.
///
/// # Panics
/// Panics if enablement drops a reason or returns the readiness bit as work.
#[def_test]
fn bottom_half_and_existing_reasons_survive_boot_drain() {
    let pending = PendingWord::new();
    let reasons = NotificationFlags::BOTTOM_HALF
        | NotificationFlags::RCU
        | NotificationFlags::HIGHPRI_BOTTOM_HALF
        | NotificationFlags::TIMER_REPROGRAM
        | NotificationFlags::RESCHEDULE;
    for reason in reasons.iter() {
        assert!(!pending.request(reason));
    }
    assert!(pending.take().is_empty());
    pending.enable();
    assert_eq!(pending.take(), reasons);
    assert!(pending.take().is_empty());
}

/// Requests racing an earlier BH drain remain independent and observable.
///
/// # Panics
/// Panics if a coalesced or post-drain category is lost.
#[def_test]
fn bottom_half_coalesces_without_losing_requests_during_service() {
    let pending = PendingWord::new();
    pending.enable();
    for reason in [
        NotificationFlags::BOTTOM_HALF,
        NotificationFlags::HIGHPRI_BOTTOM_HALF,
    ] {
        assert!(pending.request(reason));
        assert!(pending.request(reason));
        assert_eq!(pending.take(), reason);
        assert!(pending.request(reason));
        assert!(pending.request(NotificationFlags::TIMER_REPROGRAM));
        assert_eq!(pending.take(), reason | NotificationFlags::TIMER_REPROGRAM);
        assert!(pending.take().is_empty());
    }
}
