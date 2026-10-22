// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{BoottimeInstant, SystemTime};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn mono(seconds: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(seconds))
}

#[def_test]
fn begin_saves_snapshot_without_freezing_or_changing_clocks() {
    let mut timekeeper = Timekeeper::INITIAL;
    let before = timekeeper.snapshot(mono(10));
    let token = timekeeper.begin_accounting(mono(10), 17).unwrap();
    let reading = token.reading;
    assert_eq!(reading.monotonic, before.monotonic);
    assert_eq!(reading.realtime, before.realtime);
    assert_eq!(reading.boottime, before.boottime);
    assert_eq!(timekeeper.snapshot(mono(11)).realtime, SystemTime::from_unix_seconds(11));
    let state = timekeeper;
    assert!(matches!(timekeeper.begin_accounting(mono(11), 18),
        Err(SuspendAccountingError::AlreadyActive)));
    assert_eq!(timekeeper, state);
}

#[def_test]
fn resume_injects_both_offsets_once_and_keeps_monotonic() {
    let mut timekeeper = Timekeeper::INITIAL;
    let token = timekeeper.begin_accounting(mono(10), 0).unwrap();
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::from_secs(30), mono(12)), Ok(()));
    let reading = timekeeper.snapshot(mono(12));
    assert_eq!(reading.monotonic, mono(12));
    assert_eq!(reading.realtime, SystemTime::from_unix_seconds(42));
    assert_eq!(reading.boottime,
        BoottimeInstant::from_span_since_origin(TimeSpan::from_secs(42)));
    let committed = timekeeper;
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::from_secs(30), mono(12)),
        Err(SuspendAccountingError::StaleToken));
    assert_eq!(timekeeper.cancel_accounting(&token), Err(SuspendAccountingError::StaleToken));
    assert_eq!(timekeeper, committed);
}

#[def_test]
fn cancellation_and_old_token_cannot_settle_a_later_interval() {
    let mut timekeeper = Timekeeper::INITIAL;
    let old = timekeeper.begin_accounting(mono(2), 0).unwrap();
    assert_eq!(timekeeper.cancel_accounting(&old), Ok(()));
    assert_eq!(timekeeper.sleep_time, TimeSpan::ZERO);
    let current = timekeeper.begin_accounting(mono(3), 0).unwrap();
    let before = timekeeper;
    assert_eq!(timekeeper.resume_accounting(&old, TimeSpan::from_secs(1), mono(4)),
        Err(SuspendAccountingError::StaleToken));
    assert_eq!(timekeeper.cancel_accounting(&old), Err(SuspendAccountingError::StaleToken));
    assert_eq!(timekeeper, before);
    assert_eq!(timekeeper.resume_accounting(&current, TimeSpan::from_secs(1), mono(4)), Ok(()));
}

#[def_test]
fn overflow_retains_active_token_for_retry_without_partial_commit() {
    let mut timekeeper = Timekeeper::INITIAL;
    let token = timekeeper.begin_accounting(mono(2), 0).unwrap();
    let before = timekeeper;
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::MAX, mono(3)),
        Err(SuspendAccountingError::SleepTime(SleepTimeError::OutOfRange)));
    assert_eq!(timekeeper, before);
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::from_secs(1), mono(3)), Ok(()));
}

#[def_test]
fn invalid_restored_baseline_does_not_consume_even_a_zero_resume() {
    let mut timekeeper = Timekeeper::INITIAL;
    let token = timekeeper.begin_accounting(mono(10), 0).unwrap();
    let before = timekeeper;
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::ZERO, mono(9)),
        Err(SuspendAccountingError::SleepTime(SleepTimeError::InvalidMonotonicBaseline)));
    assert_eq!(timekeeper, before);
    assert_eq!(timekeeper.cancel_accounting(&token), Ok(()));
}

#[def_test]
fn untracked_nonzero_injection_is_rejected_while_accounting_is_active() {
    let mut timekeeper = Timekeeper::INITIAL;
    let token = timekeeper.begin_accounting(mono(2), 0).unwrap();
    let before = timekeeper;
    assert_eq!(timekeeper.inject_sleeptime(TimeSpan::from_secs(1), mono(3)),
        Err(SleepTimeError::AccountingInProgress));
    assert_eq!(timekeeper.inject_sleeptime(TimeSpan::ZERO, mono(3)), Ok(()));
    assert_eq!(timekeeper, before);
    assert_eq!(timekeeper.cancel_accounting(&token), Ok(()));
    assert_eq!(timekeeper.inject_sleeptime(TimeSpan::from_secs(1), mono(3)), Ok(()));
}

#[def_test]
fn wallclock_update_during_accounting_is_preserved() {
    let mut timekeeper = Timekeeper::INITIAL;
    let token = timekeeper.begin_accounting(mono(10), 0).unwrap();
    timekeeper.set_realtime_checked(SystemTime::from_unix_seconds(1000), mono(11)).unwrap();
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::from_secs(30), mono(12)), Ok(()));
    assert_eq!(timekeeper.snapshot(mono(12)).realtime, SystemTime::from_unix_seconds(1031));
    assert_eq!(token.reading.realtime, SystemTime::from_unix_seconds(10));
}

#[def_test]
fn zero_resume_closes_transaction_without_requesting_notification() {
    let mut timekeeper = Timekeeper::INITIAL;
    let token = timekeeper.begin_accounting(mono(2), 0).unwrap();
    let before = timekeeper.snapshot(mono(3));
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::ZERO, mono(3)), Ok(()));
    assert_eq!(timekeeper.snapshot(mono(3)), before);
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::ZERO, mono(3)),
        Err(SuspendAccountingError::StaleToken));
    assert!(timekeeper.begin_accounting(mono(4), 0).is_ok());
}

#[def_test]
fn identities_never_wrap_or_reactivate_old_tokens() {
    let mut timekeeper = Timekeeper::INITIAL;
    timekeeper.suspend.last_id = u64::MAX - 1;
    let token = timekeeper.begin_accounting(mono(2), 0).unwrap();
    assert_eq!(timekeeper.cancel_accounting(&token), Ok(()));
    let before = timekeeper;
    assert!(matches!(timekeeper.begin_accounting(mono(3), 0),
        Err(SuspendAccountingError::IdentityExhausted)));
    assert_eq!(timekeeper, before);
}

#[def_test]
fn restored_baseline_must_also_cover_a_later_wallclock_correlation() {
    let mut timekeeper = Timekeeper::INITIAL;
    let token = timekeeper.begin_accounting(mono(10), 0).unwrap();
    timekeeper.set_realtime_checked(SystemTime::from_unix_seconds(100), mono(20)).unwrap();
    let before = timekeeper;
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::ZERO, mono(15)),
        Err(SuspendAccountingError::SleepTime(SleepTimeError::InvalidMonotonicBaseline)));
    assert_eq!(timekeeper, before);
    assert_eq!(timekeeper.resume_accounting(&token, TimeSpan::ZERO, mono(20)), Ok(()));
}


#[def_test]
fn dropping_ownership_cancels_without_clock_changes_and_allows_retry() {
    let keeper = SpinRwNoIrq::new(Timekeeper::INITIAL);
    let token = keeper.write().begin_accounting(mono(10), 0).unwrap();
    let before = keeper.read().snapshot(mono(11));
    let ownership = AccountingOwnership { token, keeper: &keeper };
    drop(ownership);
    assert_eq!(keeper.read().snapshot(mono(11)), before);
    assert!(!keeper.read().suspend.is_active());
    assert_eq!(keeper.write().inject_sleeptime(TimeSpan::from_secs(1), mono(11)), Ok(()));
    let token = keeper.write().begin_accounting(mono(12), 0).unwrap();
    drop(AccountingOwnership { token, keeper: &keeper });
    assert!(!keeper.read().suspend.is_active());
}

#[def_test]
fn dropping_completed_or_cancelled_ownership_does_not_cancel_next_interval() {
    let keeper = SpinRwNoIrq::new(Timekeeper::INITIAL);
    let old = keeper.write().begin_accounting(mono(10), 0).unwrap();
    let ownership = AccountingOwnership { token: old, keeper: &keeper };
    assert_eq!(keeper.write().resume_accounting(&ownership.token, TimeSpan::from_secs(1), mono(11)), Ok(()));
    let current = keeper.write().begin_accounting(mono(12), 0).unwrap();
    let before = *keeper.read();
    drop(ownership);
    assert_eq!(*keeper.read(), before);
    let ownership = AccountingOwnership { token: current, keeper: &keeper };
    assert_eq!(keeper.write().cancel_accounting(&ownership.token), Ok(()));
    let next = keeper.write().begin_accounting(mono(13), 0).unwrap();
    let before = *keeper.read();
    drop(ownership);
    assert_eq!(*keeper.read(), before);
    drop(AccountingOwnership { token: next, keeper: &keeper });
}

#[def_test]
fn dropping_after_failed_resume_releases_only_ownership() {
    let keeper = SpinRwNoIrq::new(Timekeeper::INITIAL);
    let token = keeper.write().begin_accounting(mono(10), 0).unwrap();
    let ownership = AccountingOwnership { token, keeper: &keeper };
    assert!(keeper.write().resume_accounting(&ownership.token, TimeSpan::MAX, mono(11)).is_err());
    let before = keeper.read().snapshot(mono(11));
    drop(ownership);
    assert_eq!(keeper.read().snapshot(mono(11)), before);
    assert!(!keeper.read().suspend.is_active());
}


// Dedicated keeper lives for the test run, matching the public token's static
// owner lifetime. Token Drop clears its active interval even on assertion failure.
// It never writes the kernel's global keeper or clock-change notification state.
static DEBUG_KEEPER: SpinRwNoIrq<Timekeeper> = SpinRwNoIrq::new(Timekeeper::INITIAL);

#[def_test(serial)]
fn public_token_debug_uses_public_type_name() {
    let credential = DEBUG_KEEPER.write().begin_accounting(mono(10), 0).unwrap();
    let token = SuspendToken {
        ownership: AccountingOwnership { token: credential, keeper: &DEBUG_KEEPER },
    };
    let text = alloc::format!("{token:?}");
    assert!(text.starts_with("SuspendToken {"));
    assert!(!text.contains("AccountingToken"));
    assert_eq!(token.reading().monotonic, mono(10));
}
