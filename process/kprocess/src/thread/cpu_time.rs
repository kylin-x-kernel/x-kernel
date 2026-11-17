// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread CPU-time accounting state.

use core::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};

use ktime_types::{MonotonicInstant, TimeSpan};

/// Represents the current CPU-accounting mode of a thread.
#[repr(u8)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CpuTimeState {
    /// The thread is off CPU or has finished CPU accounting.
    Inactive = 0,
    /// The thread is executing in user space.
    User     = 1,
    /// The thread is executing in kernel space.
    Kernel   = 2,
}

impl CpuTimeState {
    const fn from_raw(raw: u8) -> Self {
        match raw {
            1 => Self::User,
            2 => Self::Kernel,
            _ => Self::Inactive,
        }
    }
}

/// Per-thread sampled CPU-time accounting, following Linux generic vtime.
///
/// The user/kernel boundary only publishes the new mode with one relaxed store.
/// Accounting is settled later, at a timer tick, a context switch or a query,
/// so the syscall path never takes a lock or reads the clock. Settlement charges
/// the elapsed interval to the mode that is current when it runs, giving the
/// `utime`/`stime` split tick-level resolution: long kernel work still settles
/// as kernel time because it runs with [`CpuTimeState::Kernel`] published.
///
/// Every field is atomic because a query can sample another thread's counters
/// from a different CPU. Settlement is a read-modify-write serialized by a
/// compare-exchange on [`Self::last_wall_ns`], so two concurrent settlements of
/// the same interval cannot both charge it.
pub(crate) struct CpuTimeStatistics {
    utime_ns: AtomicU64,
    stime_ns: AtomicU64,
    /// Wall-clock origin of the unsettled interval. `0` before the first
    /// switch-in; a settle from there simply establishes the origin.
    last_wall_ns: AtomicU64,
    mode: AtomicU8,
    /// Set when exit publishes the final sample. Switch-in must not reopen
    /// an interval after that; `Thread::is_exiting` is raised earlier for PI.
    finalized: AtomicBool,
}

impl CpuTimeStatistics {
    /// Creates inactive accounting; the first switch-in establishes its origin.
    pub(crate) const fn new() -> Self {
        Self {
            utime_ns: AtomicU64::new(0),
            stime_ns: AtomicU64::new(0),
            last_wall_ns: AtomicU64::new(0),
            mode: AtomicU8::new(CpuTimeState::Inactive as u8),
            finalized: AtomicBool::new(false),
        }
    }

    /// Returns the committed user and kernel CPU time.
    pub(crate) fn output(&self) -> (TimeSpan, TimeSpan) {
        (
            TimeSpan::from_nanos(self.utime_ns.load(Ordering::Relaxed)),
            TimeSpan::from_nanos(self.stime_ns.load(Ordering::Relaxed)),
        )
    }

    /// Publishes the mode of the context that is about to run.
    ///
    /// Called at each user/kernel boundary. This is one relaxed store; the
    /// elapsed interval is settled by the next tick, switch or query.
    pub(crate) fn record_mode(&self, state: CpuTimeState) {
        self.mode.store(state as u8, Ordering::Relaxed);
    }

    /// Settles the interval started at the previous settlement into the mode
    /// that is currently published.
    pub(crate) fn settle(&self, now: MonotonicInstant) {
        let now_ns = now.as_nanos_u64_saturating();
        loop {
            let last_ns = self.last_wall_ns.load(Ordering::Acquire);
            if now_ns <= last_ns {
                return;
            }
            if self
                .last_wall_ns
                .compare_exchange(last_ns, now_ns, Ordering::AcqRel, Ordering::Acquire)
                .is_err()
            {
                continue;
            }
            let delta_ns = now_ns - last_ns;
            match CpuTimeState::from_raw(self.mode.load(Ordering::Relaxed)) {
                CpuTimeState::User => {
                    self.utime_ns.fetch_add(delta_ns, Ordering::Relaxed);
                }
                CpuTimeState::Kernel => {
                    self.stime_ns.fetch_add(delta_ns, Ordering::Relaxed);
                }
                CpuTimeState::Inactive => {}
            }
            return;
        }
    }

    /// Starts a fresh interval for a thread that has just been switched in.
    ///
    /// The off-CPU gap since [`Self::deactivate`] is discarded, not charged.
    pub(crate) fn activate(&self, now: MonotonicInstant) {
        // Exit raises `is_exiting` before PI cleanup, which can still be
        // scheduled. Only the final publication refuses a new interval.
        if self.is_final() {
            return;
        }
        self.last_wall_ns
            .store(now.as_nanos_u64_saturating(), Ordering::Release);
        self.record_mode(CpuTimeState::Kernel);
    }

    /// Closes accounting for exit. Later switch-ins do not reopen it.
    pub(crate) fn finalize(&self, now: MonotonicInstant) {
        self.finalized.store(true, Ordering::Release);
        self.deactivate(now);
    }

    pub(crate) fn is_final(&self) -> bool {
        self.finalized.load(Ordering::Acquire)
    }

    /// Settles the running interval and marks the thread inactive.
    pub(crate) fn deactivate(&self, now: MonotonicInstant) {
        self.settle(now);
        self.record_mode(CpuTimeState::Inactive);
    }

    /// Settles up to `now` and returns the accumulated user and kernel time.
    pub(crate) fn sample(&self, now: MonotonicInstant) -> (TimeSpan, TimeSpan) {
        self.settle(now);
        self.output()
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/thread_cpu_time.rs");
