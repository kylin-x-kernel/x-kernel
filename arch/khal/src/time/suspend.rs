// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Explicit system-wide monotonic freeze and counter rebase for a future PM owner.

pub use super::epoch::MonotonicSuspendError;
use super::{ClockEventIf, CounterSourceIf, epoch::ClockEpoch};

pub(super) static CLOCK: ClockEpoch = ClockEpoch::new();

/// Authority to resume one frozen monotonic-clock interval.
///
/// Keep this credential until resume succeeds, including suspend-abort paths.
/// Drop does not resume hardware or the clock: those require an explicit PM
/// decision after counter access is restored. Losing it leaves the clock frozen.
#[derive(Debug)]
#[must_use = "retain the token and explicitly resume the clock, even when PM aborts"]
pub struct MonotonicSuspendToken {
    identity: u64,
}

/// Freezes the system-wide logical monotonic clock at the current counter sample.
///
/// The PM owner must park other CPUs and stop clock consumers, timer programming,
/// watchdogs and NMI clock reads before entry. Each CPU's local event must be
/// disarmed separately; this entry also disarms the coordinating CPU's event
/// after sampling, including any event rearmed by legacy counter repair.
/// Call after timer initialization, on the coordinating CPU; does not sleep,
/// allocate, stop devices, or change ktime accounting.
/// IRQs/preemption are masked for the short mapping update. Reads may run on
/// the owner after return, but busy waits and timeouts cannot progress while frozen.
/// No production PM path is connected by this API.
///
/// # Errors
/// Reports an active suspend, a concurrent transition or exhausted identities,
/// without changing the clock mapping.
pub fn suspend_monotonic_clock() -> Result<MonotonicSuspendToken, MonotonicSuspendError> {
    let _guard = kspin::NoPreemptIrqSave::new();
    let identity = CLOCK.suspend(|| CounterSourceIf::now_ticks().as_raw())?;
    ClockEventIf::disarm_timer();
    Ok(MonotonicSuspendToken { identity })
}

/// Re-bases the clock to the restored counter, excluding the frozen interval.
///
/// The same PM owner must restore counter access and keep its frequency stable,
/// with other CPUs, NMI clock readers and timer programming still quiesced.
/// Counter progress, stoppage or reset during the frozen interval is discarded.
/// Also use this on abort after freezing: the omitted interval needs a trusted
/// elapsed-time source if PM wants to compensate it. This API does not measure
/// sleep or update REALTIME/BOOTTIME; settle ktime accounting separately, then
/// restore local clock-event hardware and re-arbitrate deadlines on every CPU
/// before releasing normal execution. Do not hold a lock needed by the counter
/// backend. Does not sleep or allocate; masks local IRQs/preemption briefly.
///
/// # Errors
/// A stale token, concurrent transition or identity exhaustion leaves the
/// mapping unchanged. An active owner retains its credential for retry.
pub fn resume_monotonic_clock(token: &MonotonicSuspendToken) -> Result<(), MonotonicSuspendError> {
    let _guard = kspin::NoPreemptIrqSave::new();
    CLOCK.resume(token.identity, || CounterSourceIf::resume_ticks().as_raw())
}
