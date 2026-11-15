// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Cgroup CPU bandwidth accounting (`cpu.max` quota/period state machine).
//!
//! One [`Bandwidth`] instance backs one cgroup's `cpu.max`. Scheduler ticks
//! charge runtime into the current period; a charge that crosses the quota
//! marks the group throttled until the period rolls over. Rollover is
//! detected lazily against the monotonic clock, so an idle CPU still closes
//! throttle windows: callers poll each tick regardless of charge activity.
//!
//! The state machine carries no task identities. Callers own the member list
//! and translate `charge`/`poll` outcomes into scheduling decisions (weight
//! collapse, queue parking, counters), keeping this crate free of scheduler
//! dependencies.

#![no_std]

extern crate alloc;

use core::sync::atomic::{AtomicU64, Ordering};

use kspin::SpinNoIrq;

/// Linux's smallest accepted bandwidth period, in microseconds.
pub const PERIOD_MIN_US: u64 = 1_000;
/// Linux's largest accepted bandwidth period, in microseconds.
pub const PERIOD_MAX_US: u64 = 1_000_000;
/// Linux's smallest accepted non-max quota, in microseconds.
pub const QUOTA_MIN_US: u64 = 1_000;
/// Default period for `cpu.max` = "max 100000".
pub const DEFAULT_PERIOD_US: u64 = 100_000;

const QUOTA_MAX_SENTINEL: u64 = u64::MAX;

/// Bandwidth accounting for one cgroup.
pub struct Bandwidth {
    quota_us: AtomicU64,
    period_us: AtomicU64,
    nr_periods: AtomicU64,
    nr_throttled: AtomicU64,
    throttled_usec: AtomicU64,
    /// Cumulative charged runtime across periods, in microseconds.
    usage_us: AtomicU64,
    state: SpinNoIrq<State>,
    closed: core::sync::atomic::AtomicBool,
}

#[derive(Default)]
struct State {
    /// Monotonic time (ns) when the current period started; zero = never charged.
    anchor_ns: u64,
    /// Runtime charged inside the current period, in microseconds.
    runtime_us: u64,
    /// Whether members are currently denied their share.
    throttled: bool,
    /// Monotonic time (ns) when the current throttle window began.
    throttled_since_ns: u64,
}

impl Default for Bandwidth {
    fn default() -> Self {
        Self::new()
    }
}

impl Bandwidth {
    /// Creates a tracker with `cpu.max` = "max 100000".
    pub fn new() -> Self {
        Self {
            quota_us: AtomicU64::new(QUOTA_MAX_SENTINEL),
            period_us: AtomicU64::new(DEFAULT_PERIOD_US),
            nr_periods: AtomicU64::new(0),
            nr_throttled: AtomicU64::new(0),
            throttled_usec: AtomicU64::new(0),
            usage_us: AtomicU64::new(0),
            state: SpinNoIrq::new(State::default()),
            closed: core::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Returns the quota, or `None` for "max".
    pub fn quota_us(&self) -> Option<u64> {
        let quota = self.quota_us.load(Ordering::Acquire);
        (quota != QUOTA_MAX_SENTINEL).then_some(quota)
    }

    /// Returns the period.
    pub fn period_us(&self) -> u64 {
        self.period_us.load(Ordering::Acquire)
    }

    /// Stores the quota/period pair.
    ///
    /// Returns `false` when either value is outside the Linux `cpu.max`
    /// domain; the previous pair is kept unchanged.
    pub fn set(&self, quota_us: Option<u64>, period_us: u64) -> bool {
        if !(PERIOD_MIN_US..=PERIOD_MAX_US).contains(&period_us) {
            return false;
        }
        if let Some(quota) = quota_us {
            if quota < QUOTA_MIN_US {
                return false;
            }
        }
        self.quota_us
            .store(quota_us.unwrap_or(QUOTA_MAX_SENTINEL), Ordering::Release);
        self.period_us.store(period_us, Ordering::Release);
        true
    }

    /// Returns `(nr_periods, nr_throttled, throttled_usec)`.
    pub fn counters(&self) -> (u64, u64, u64) {
        (
            self.nr_periods.load(Ordering::Acquire),
            self.nr_throttled.load(Ordering::Acquire),
            self.throttled_usec.load(Ordering::Acquire),
        )
    }

    /// Returns the runtime charged in the current period, in microseconds.
    pub fn runtime_us(&self) -> u64 {
        self.state.lock().runtime_us
    }

    /// Returns the cumulative charged runtime, in microseconds.
    pub fn total_usage_us(&self) -> u64 {
        self.usage_us.load(Ordering::Acquire)
    }

    /// Marks the tracker closed after its controller is disabled.
    pub fn close(&self) {
        self.closed.store(true, Ordering::Release);
    }

    /// Reopens a tracker after its quota changed while throttled.
    pub fn reopen(&self) {
        self.closed.store(false, Ordering::Release);
        let mut state = self.state.lock();
        state.throttled = false;
        state.runtime_us = 0;
        state.anchor_ns = 0;
    }

    /// Reports whether the tracker has been retired.
    pub fn is_closed(&self) -> bool {
        self.closed.load(Ordering::Acquire)
    }

    /// Reports whether members are currently throttled.
    pub fn is_throttled(&self) -> bool {
        self.state.lock().throttled
    }

    /// Returns the monotonic time (ns) the current period started at.
    ///
    /// Zero means "never charged": the first charge or poll initializes the
    /// anchor. Callers that drive the tracker with synthetic timestamps read
    /// this after initialization instead of assuming the clock starts at zero.
    pub fn period_anchor_ns(&self) -> u64 {
        self.state.lock().anchor_ns
    }

    /// Charges runtime into the current period.
    ///
    /// Returns `true` when this charge *started* a throttle window (quota
    /// exceeded). The caller translates that into scheduling state for the
    /// group's members; repeat charges while already throttled return `false`
    /// so member state is only mutated on the rising edge.
    pub fn charge(&self, runtime_us: u64, now_ns: u64) -> bool {
        let mut state = self.state.lock();
        self.roll_over(&mut state, now_ns);
        state.runtime_us = state.runtime_us.saturating_add(runtime_us);
        self.usage_us.fetch_add(runtime_us, Ordering::AcqRel);
        let quota = self.quota_us.load(Ordering::Acquire);
        if quota == QUOTA_MAX_SENTINEL || state.throttled || state.runtime_us < quota {
            return false;
        }
        state.throttled = true;
        state.throttled_since_ns = now_ns;
        self.nr_throttled.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// Advances the period without charging.
    ///
    /// Returns `true` when this poll *ended* a throttle window; the caller
    /// then resumes the group's members. Returns `false` while the current
    /// period is still open.
    pub fn poll(&self, now_ns: u64) -> bool {
        let mut state = self.state.lock();
        if state.anchor_ns == 0 {
            state.anchor_ns = now_ns;
            return false;
        }
        let elapsed = self.roll_over(&mut state, now_ns);
        if elapsed > 0 && state.throttled {
            state.throttled = false;
            self.throttled_usec.fetch_add(
                now_ns.saturating_sub(state.throttled_since_ns) / 1_000,
                Ordering::AcqRel,
            );
            return true;
        }
        false
    }

    /// Rolls expired periods forward. Returns the number of elapsed periods.
    fn roll_over(&self, state: &mut State, now_ns: u64) -> u64 {
        if state.anchor_ns == 0 {
            // First observation starts the period; zero is the "never charged"
            // sentinel so the guest's nonzero monotonic clock cannot subtract
            // across it.
            state.anchor_ns = now_ns;
            return 0;
        }
        let period_ns = self.period_us.load(Ordering::Acquire).saturating_mul(1_000);
        if period_ns == 0 || now_ns < state.anchor_ns.saturating_add(period_ns) {
            return 0;
        }
        let elapsed_periods = (now_ns - state.anchor_ns) / period_ns;
        state.anchor_ns += elapsed_periods * period_ns;
        state.runtime_us = 0;
        self.nr_periods.fetch_add(elapsed_periods, Ordering::AcqRel);
        elapsed_periods
    }
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
