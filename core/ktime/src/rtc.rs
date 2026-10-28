// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Persistent-clock fallback for one explicitly coordinated suspend interval.

use kiface_time_api::PersistentClock;
use ktime_types::{NANOS_PER_SEC, SystemTime, TimeSpan};

use super::{ClockReading, SuspendAccountingError, SuspendToken};

/// Why RTC-based sleep accounting could not proceed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RtcSleepError {
    /// Resolution, sleep bound or sampling budget is zero.
    InvalidPolicy,
    /// The bound clock no longer advertises the same timestamp resolution.
    SourceChanged,
    /// The selected RTC returned no usable reading.
    ReadFailed,
    /// Sampling exceeded the caller's maximum acceptable read window.
    ReadTooSlow,
    /// MONOTONIC moved backwards while sampling or across the interval.
    InvalidMonotonic,
    /// The RTC moved backwards across the interval.
    RtcWentBackwards,
    /// RTC progress is insufficient beyond the sampling/quantization allowance.
    InsufficientRtcProgress,
    /// Estimated missing time exceeds the PM owner's declared sleep bound.
    SleepTooLong,
    /// The PM owner has not confirmed reaching the monotonic freeze boundary.
    NotSuspended,
    /// The freeze boundary was already recorded for this session.
    AlreadySuspended,
    /// This session has already settled or been cancelled.
    Finished,
    /// The underlying single-use accounting transaction rejected the operation.
    Accounting(SuspendAccountingError),
}

#[derive(Clone, Copy, Debug)]
struct Policy {
    resolution: TimeSpan,
    max_sleep: TimeSpan,
    max_read_span: TimeSpan,
}

#[derive(Clone, Copy, Debug)]
struct Sample {
    rtc: SystemTime,
    clocks: ClockReading,
    read_span: TimeSpan,
}

#[derive(Clone, Copy, Debug)]
struct Estimate {
    delta: TimeSpan,
    phase: i128,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum FreezeBoundary {
    NotReached,
    Reached,
}

impl FreezeBoundary {
    fn enter(&mut self) -> Result<(), RtcSleepError> {
        if *self == Self::Reached {
            return Err(RtcSleepError::AlreadySuspended);
        }
        *self = Self::Reached;
        Ok(())
    }

    fn require_reached(self) -> Result<(), RtcSleepError> {
        match self {
            Self::NotReached => Err(RtcSleepError::NotSuspended),
            Self::Reached => Ok(()),
        }
    }
}

/// Owns one stable persistent-clock binding and its quantization calibration.
///
/// PM first prefers a trustworthy nonstop/persistent source. This is the RTC
/// fallback, not a source-discovery registry. Like Linux rtc_suspend/resume, it
/// removes running time and keeps a cross-cycle phase reference to avoid adding
/// a fresh rounding error every cycle. BOOTTIME replaces Linux's wall-clock
/// sample so settimeofday does not perturb the running-time calculation.
/// No production PM or host protocol is installed by constructing this value.
#[derive(Debug)]
pub struct RtcSleepAccounting<C> {
    clock: C,
    policy: Policy,
    phase: Option<i128>,
}

/// Borrows the same clock through sampling, settlement or cancellation.
///
/// Drop cancels pending ktime accounting and discards phase calibration.
/// PM must independently
/// restore MONOTONIC and hardware even on abort. This session never freezes
/// clocks, stops CPUs or restores devices. Do not forget the session, drop it
/// from NMI, or hold timekeeping/observer locks during settlement or Drop.
#[derive(Debug)]
#[must_use = "retain the session until explicit settlement or cancellation"]
pub struct RtcSleepSession<'a, C> {
    owner: &'a mut RtcSleepAccounting<C>,
    token: Option<SuspendToken>,
    before: Sample,
    boundary: FreezeBoundary,
}

impl<C: PersistentClock> RtcSleepAccounting<C> {
    /// Configures RTC fallback with caller-selected bounds.
    ///
    /// `max_sleep` bounds RTC-estimated compensation, not wall-clock time spent in
    /// this Rust call. `max_read_span` bounds observed sample latency; it cannot
    /// interrupt a backend that never returns. The source must remain trusted,
    /// stable and readable in the caller's PM phase. No I/O occurs here.
    ///
    /// # Errors
    /// Returns InvalidPolicy for a zero quantum, sleep bound or sampling budget.
    pub fn new(
        clock: C,
        max_sleep: TimeSpan,
        max_read_span: TimeSpan,
    ) -> Result<Self, RtcSleepError> {
        let resolution = clock.resolution();
        if resolution.is_zero() || max_sleep.is_zero() || max_read_span.is_zero() {
            return Err(RtcSleepError::InvalidPolicy);
        }
        Ok(Self {
            clock,
            policy: Policy {
                resolution,
                max_sleep,
                max_read_span,
            },
            phase: None,
        })
    }

    fn commit_phase(
        &mut self,
        delta: TimeSpan,
        phase: Option<i128>,
        commit: impl FnOnce(TimeSpan) -> Result<(), SuspendAccountingError>,
    ) -> Result<(), RtcSleepError> {
        commit(delta).map_err(RtcSleepError::Accounting)?;
        self.phase = phase;
        Ok(())
    }

    /// Discards phase calibration after a known persistent-clock adjustment.
    /// Call between sessions; the session's exclusive borrow prevents changes
    /// during an active measurement. Does not read hardware or change time.
    pub fn reset_calibration(&mut self) {
        self.phase = None;
    }

    /// Starts ktime accounting and samples the bound RTC before actual freeze.
    ///
    /// Requires a sleepable context allowed by the device, no spinlock held,
    /// initialized timer access and MONOTONIC still running. Mapping/discovery
    /// must already be complete. The exclusive borrow keeps the same binding
    /// through the cycle. A failed initial sample drops/cancels the transaction
    /// without changing calibration or any clock. PM may instead use ordinary
    /// ktime accounting directly when a preferred source is already available.
    ///
    /// # Errors
    /// Reports transaction contention or invalid/failed/slow sampling.
    pub fn begin(&mut self) -> Result<RtcSleepSession<'_, C>, RtcSleepError> {
        let token = super::begin_suspend_accounting().map_err(RtcSleepError::Accounting)?;
        let before = capture(&mut self.clock, self.policy, super::clock_reading)?;
        Ok(RtcSleepSession {
            owner: self,
            token: Some(token),
            before,
            boundary: FreezeBoundary::NotReached,
        })
    }
}

impl<C: PersistentClock> RtcSleepSession<'_, C> {
    /// Records that PM successfully reached its monotonic freeze boundary.
    ///
    /// Call only after the platform has frozen MONOTONIC, before entering sleep.
    /// This is a PM assertion, not hardware detection. It performs no I/O,
    /// allocation or notification and is valid with IRQs masked. Early aborts
    /// that never reach this point must cancel rather than estimate sleep.
    ///
    /// # Errors
    /// Finished rejects a closed session; AlreadySuspended rejects duplicates.
    pub fn mark_suspended(&mut self) -> Result<(), RtcSleepError> {
        self.token.as_ref().ok_or(RtcSleepError::Finished)?;
        self.boundary.enter()
    }

    /// Estimates and commits the confirmed missing interval using RTC fallback.
    ///
    /// PM must have passed a real suspend boundary and restored continuous
    /// MONOTONIC excluding sleep before entry. Read in the device's sleepable
    /// context, outside spinlocks, and restore event hardware/deadlines through
    /// the separate PM sequence. The same source must advance across the target
    /// sleep. Plausible forward RTC edits within max_sleep cannot be detected;
    /// PM must prevent RTC writes and establish source trust.
    ///
    /// # Errors
    /// Invalid or failed samples and rejected commits retain the transaction and
    /// prior calibration for retry/cancel. A settled/cancelled session returns
    /// Finished before reading hardware. Successful nonzero commits use existing
    /// ktime notifications; this method issues no independent second injection.
    pub fn resume_from_rtc(&mut self) -> Result<TimeSpan, RtcSleepError> {
        let token = self.token.as_ref().ok_or(RtcSleepError::Finished)?;
        self.boundary.require_reached()?;
        let after = capture(
            &mut self.owner.clock,
            self.owner.policy,
            super::clock_reading,
        )?;
        let estimate = estimate(self.before, after, self.owner.phase, self.owner.policy)?;
        self.owner
            .commit_phase(estimate.delta, Some(estimate.phase), |delta| {
                super::resume_suspend_accounting(token, delta)
            })?;
        drop(self.token.take());
        Ok(estimate.delta)
    }

    /// Settles once with a confirmed preferred-source interval, skipping RTC.
    ///
    /// The PM owner chooses source priority (e.g. nonstop counter, persistent
    /// clock, then RTC). Supply missing sleep only after restoring MONOTONIC.
    /// Success invalidates RTC phase calibration and prevents RTC fallback from
    /// injecting again. This does not require a successful resume RTC read.
    ///
    /// # Errors
    /// Failed commits keep the session active. Finished means it was already
    /// settled/cancelled. Context/notification constraints match ktime resume.
    pub fn resume_with_confirmed_time(&mut self, delta: TimeSpan) -> Result<(), RtcSleepError> {
        let token = self.token.as_ref().ok_or(RtcSleepError::Finished)?;
        self.boundary.require_reached()?;
        self.owner.commit_phase(delta, None, |delta| {
            super::resume_suspend_accounting(token, delta)
        })?;
        drop(self.token.take());
        Ok(())
    }

    /// Cancels accounting and discards calibration without reading the RTC.
    /// Does not restore hardware or compensate any omitted frozen duration.
    ///
    /// # Errors
    /// Finished means this session has already settled/cancelled; transaction
    /// errors leave the session active. No clock-change notification is emitted.
    pub fn cancel(&mut self) -> Result<(), RtcSleepError> {
        let token = self.token.as_ref().ok_or(RtcSleepError::Finished)?;
        super::cancel_suspend_accounting(token).map_err(RtcSleepError::Accounting)?;
        self.owner.phase = None;
        drop(self.token.take());
        Ok(())
    }
}

impl<C> Drop for RtcSleepSession<'_, C> {
    fn drop(&mut self) {
        if self.token.is_some() {
            // An abandoned interval must not bias a later sleep estimate.
            self.owner.phase = None;
        }
        // The token field's normal Drop cancels the ktime transaction.
    }
}

fn capture<C: PersistentClock>(
    clock: &mut C,
    policy: Policy,
    mut reading: impl FnMut() -> ClockReading,
) -> Result<Sample, RtcSleepError> {
    if clock.resolution() != policy.resolution {
        return Err(RtcSleepError::SourceChanged);
    }
    let before = reading().monotonic;
    // Device I/O occurs without any timekeeper lock held.
    let rtc = clock.read().ok_or(RtcSleepError::ReadFailed)?;
    let clocks = reading();
    let read_span = clocks
        .monotonic
        .checked_duration_since(before)
        .ok_or(RtcSleepError::InvalidMonotonic)?;
    if read_span > policy.max_read_span {
        return Err(RtcSleepError::ReadTooSlow);
    }
    if clock.resolution() != policy.resolution {
        return Err(RtcSleepError::SourceChanged);
    }
    Ok(Sample {
        rtc,
        clocks,
        read_span,
    })
}

fn span_ns(span: TimeSpan) -> i128 {
    // u64 seconds plus a subsecond fraction fits comfortably in signed i128.
    i128::from(span.as_secs()) * i128::from(NANOS_PER_SEC) + i128::from(span.subsec_nanos())
}

fn estimate(
    before: Sample,
    after: Sample,
    previous_phase: Option<i128>,
    policy: Policy,
) -> Result<Estimate, RtcSleepError> {
    let running = after
        .clocks
        .monotonic
        .checked_duration_since(before.clocks.monotonic)
        .ok_or(RtcSleepError::InvalidMonotonic)?;
    let elapsed = after
        .rtc
        .duration_since(before.rtc)
        .map_err(|_| RtcSleepError::RtcWentBackwards)?;
    let offset = span_ns(before.clocks.boottime.span_since_origin()) - before.rtc.unix_nanos();
    // Linux resets its second-resolution RTC phase reference on a ~2s change.
    // Scale that rule with the declared quantum and bounded sampling windows.
    let phase_limit = 2 * (span_ns(policy.resolution) + span_ns(policy.max_read_span));
    let phase = previous_phase
        .filter(|old| (offset - *old).abs() < phase_limit)
        .unwrap_or(offset);
    let delta = span_ns(elapsed) - span_ns(running) - (offset - phase);
    let uncertainty =
        span_ns(policy.resolution) + span_ns(before.read_span) + span_ns(after.read_span);
    if delta < -uncertainty {
        return Err(RtcSleepError::InsufficientRtcProgress);
    }
    if delta > span_ns(policy.max_sleep) {
        return Err(RtcSleepError::SleepTooLong);
    }
    let delta =
        TimeSpan::try_from_nanos(delta.max(0) as u128).ok_or(RtcSleepError::SleepTooLong)?;
    Ok(Estimate { delta, phase })
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/rtc.rs");
