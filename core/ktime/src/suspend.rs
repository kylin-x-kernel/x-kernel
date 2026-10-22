// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Suspend interval accounting, independent of platform sleep and counter repair.

use core::{fmt, sync::atomic::Ordering};

use khal::time::monotonic_time;
use kspin::SpinRwNoIrq;
use ktime_types::{MonotonicInstant, TimeSpan};

use super::{
    CHANGE_ID, ClockReading, SleepTimeError, TIMEKEEPER, Timekeeper, change, observer,
    prepare_clock_update,
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) struct SuspendState {
    last_id: u64,
    active: Option<(u64, MonotonicInstant)>,
}

impl SuspendState {
    pub(super) const INITIAL: Self = Self {
        last_id: 0,
        active: None,
    };

    pub(super) fn is_active(self) -> bool {
        self.active.is_some()
    }
}

/// Opaque authority to settle or cancel one system-wide suspend interval.
///
/// Dropping this value cancels its still-active interval without changing clocks
/// or issuing notifications. A settled/cancelled token cannot cancel a later
/// interval. No lock is held across the token's lifetime. Drop briefly takes the
/// timekeeper write lock; do not drop while holding that lock or from an NMI.
/// Explicit `mem::forget` bypasses cleanup and must not be used by the PM owner.
/// The identity is not an ABI value and is never reused during this boot.
#[must_use = "dropping the token cancels its pending accounting interval"]
pub struct SuspendToken {
    ownership: AccountingOwnership<'static>,
}

impl fmt::Debug for SuspendToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SuspendToken")
            .field("id", &self.ownership.token.id)
            .field("reading", &self.ownership.token.reading)
            .finish()
    }
}

// The state-machine credential has no global destructor. Ownership binds it to
// exactly the keeper that issued it, including independently instantiated ones.
struct AccountingToken {
    id: u64,
    reading: ClockReading,
}

struct AccountingOwnership<'a> {
    token: AccountingToken,
    keeper: &'a SpinRwNoIrq<Timekeeper>,
}

impl Drop for AccountingOwnership<'_> {
    fn drop(&mut self) {
        let mut keeper = self.keeper.write();
        if keeper
            .suspend
            .active
            .is_some_and(|(id, _)| id == self.token.id)
        {
            keeper.suspend.active = None;
        }
    }
}

impl SuspendToken {
    /// Returns the consistent clock sample captured when accounting began.
    ///
    /// This is diagnostic/input context, not an automatic RTC sleep estimate.
    pub fn reading(&self) -> ClockReading {
        self.ownership.token.reading
    }
}

/// Why a suspend accounting transaction could not proceed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SuspendAccountingError {
    /// Another transaction must first be settled or cancelled.
    AlreadyActive,
    /// The token was settled/cancelled, or does not identify the active interval.
    StaleToken,
    /// All non-reusable transaction identities have been consumed.
    IdentityExhausted,
    /// Time arithmetic or the restored monotonic baseline is invalid.
    SleepTime(SleepTimeError),
}

impl Timekeeper {
    fn begin_accounting(
        &mut self,
        monotonic: MonotonicInstant,
        change_id: u64,
    ) -> Result<AccountingToken, SuspendAccountingError> {
        if self.suspend.is_active() {
            return Err(SuspendAccountingError::AlreadyActive);
        }
        let id = self
            .suspend
            .last_id
            .checked_add(1)
            .ok_or(SuspendAccountingError::IdentityExhausted)?;
        let reading = ClockReading::new(self.snapshot(monotonic), change_id);
        self.suspend = SuspendState {
            last_id: id,
            active: Some((id, monotonic)),
        };
        Ok(AccountingToken { id, reading })
    }

    fn resume_accounting(
        &mut self,
        token: &AccountingToken,
        delta: TimeSpan,
        monotonic: MonotonicInstant,
    ) -> Result<(), SuspendAccountingError> {
        let start = self.active_start(token)?;
        if monotonic < start || monotonic < self.correlation.monotonic_base {
            return Err(SuspendAccountingError::SleepTime(
                SleepTimeError::InvalidMonotonicBaseline,
            ));
        }
        // Validate on a copy: an arithmetic failure retains both the clocks
        // and the active token, allowing the caller to retry or cancel.
        let mut next = *self;
        // Settlement uses arithmetic directly; the untracked injection entry
        // retains its active-transaction guard and cannot bypass ownership.
        next.apply_sleeptime(delta, monotonic)
            .map_err(SuspendAccountingError::SleepTime)?;
        next.suspend.active = None;
        *self = next;
        Ok(())
    }

    fn active_start(
        &self,
        token: &AccountingToken,
    ) -> Result<MonotonicInstant, SuspendAccountingError> {
        match self.suspend.active {
            Some((id, start)) if id == token.id => Ok(start),
            _ => Err(SuspendAccountingError::StaleToken),
        }
    }

    fn cancel_accounting(&mut self, token: &AccountingToken) -> Result<(), SuspendAccountingError> {
        self.active_start(token)?;
        self.suspend.active = None;
        Ok(())
    }
}

/// Begins one system-wide sleep accounting interval and saves a clock snapshot.
///
/// This prepares accounting only: reads, timers and wall-clock updates continue.
/// It does not freeze MONOTONIC, stop CPUs/devices, or read an RTC. The PM owner
/// must coordinate those operations and retain the token across its sleep path.
/// Dropping an unfinished token cancels accounting only; callers must still
/// unwind their own PM preparation. No allocation or notification occurs; the
/// timekeeper lock is released before returning. Calls may be serialized across CPUs by the internal lock.
///
/// # Errors
/// Returns AlreadyActive for an outstanding transaction, or IdentityExhausted
/// rather than reusing an identity. Neither error changes clocks or state.
pub fn begin_suspend_accounting() -> Result<SuspendToken, SuspendAccountingError> {
    let token = {
        let mut timekeeper = TIMEKEEPER.write();
        timekeeper.begin_accounting(monotonic_time(), CHANGE_ID.load(Ordering::Relaxed))?
    };
    Ok(SuspendToken {
        ownership: AccountingOwnership {
            token,
            keeper: &TIMEKEEPER,
        },
    })
}

/// Settles a confirmed interval once, advancing REALTIME and BOOTTIME only.
///
/// Before calling, restore a continuous MONOTONIC baseline that excludes delta
/// and is no earlier than the begin snapshot. The caller supplies actual sleep
/// time, not an uncorrected RTC difference or total transaction elapsed time.
/// This API neither repairs counters nor proves that sleep happened. A zero
/// delta closes the transaction without notifying clock consumers. Nonzero
/// success reuses normal sleep-injection notifications after releasing the lock;
/// observers/wakers must obey their existing IRQ-compatible contracts.
///
/// Wall-clock updates during the transaction are preserved: delta is added to
/// the current correlation, not to a saved wall-clock value. Token validation,
/// arithmetic and settlement commit under the same lock. Repeated/concurrent
/// settlement of this token can succeed only once. This does not deduplicate
/// an interval later supplied through a different transaction or injection API.
///
/// # Errors
/// Returns StaleToken for a completed/cancelled/noncurrent token, or SleepTime
/// for overflow or an invalid monotonic baseline. Every error leaves state
/// unchanged; a still-active token can be retried or explicitly cancelled.
pub fn resume_suspend_accounting(
    token: &SuspendToken,
    delta: TimeSpan,
) -> Result<(), SuspendAccountingError> {
    let mut timekeeper = TIMEKEEPER.write();
    timekeeper.resume_accounting(&token.ownership.token, delta, monotonic_time())?;
    if delta.is_zero() {
        return Ok(());
    }
    let update = prepare_clock_update(&timekeeper, true);
    let wake = change::publish(true);
    drop(timekeeper);
    observer::notify(update);
    change::wake(wake);
    Ok(())
}

/// Cancels a prepared or failed interval without changing any clock.
///
/// The PM owner must decide that no sleep compensation should be committed.
/// This only releases accounting ownership, not paused hardware or tasks.
/// No clock-change notification is issued.
///
/// # Errors
/// Returns StaleToken if the token no longer identifies the active interval.
pub fn cancel_suspend_accounting(token: &SuspendToken) -> Result<(), SuspendAccountingError> {
    TIMEKEEPER.write().cancel_accounting(&token.ownership.token)
}

unittest::path_mod_test!(tests, "../ut-tests/suspend.rs");
