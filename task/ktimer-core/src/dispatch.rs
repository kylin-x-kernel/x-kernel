// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Borrowed dispatch: callbacks run outside the timer lock; storage stays external.

use intrusive_collections::UnsafeRef;
use kspin::{SpinNoIrq, SpinNoIrqGuard};

use super::cpu::CpuTimers;

/// Drains one fixed-cutoff round, completing each delivery before taking the next.
/// Returns false when nothing can be delivered. This is not a hardware IRQ loop.
/// Callback-local resources retire before clearing running identity; the
/// prepared handler and its captures remain externally owned for future calls.
/// No callback is called while holding the timer lock.
/// The initial guard is consumed so a preceding due check need not relock.
///
/// # Safety
/// The caller services the correct CPU and keeps every participating node fixed
/// and live through pending, running and all external accesses. All node changes
/// follow the same owner-lock protocol. The callback must not free its node or
/// synchronously wait for this delivery; it may rearm or try-cancel under the lock.
/// The bound handler must suit the calling context, must not
/// unwind, and must not retain node references beyond the call.
/// Each node's with_handler storage contract must remain valid through dispatch.
/// `locked` must protect `timers`, not another CPU's collection.
pub(super) unsafe fn dispatch_round(
    timers: &SpinNoIrq<CpuTimers>,
    due_at_ns: u64,
    mut locked: SpinNoIrqGuard<'_, CpuTimers>,
) -> bool {
    let mut expired = locked.take_expired(due_at_ns);
    drop(locked);
    let any_due = expired.is_some();
    while let Some((node, expiration)) = expired {
        // SAFETY: take_expired published running and the lock was released.
        // The caller preserves node and bound handler storage through delivery.
        unsafe { node.invoke(timers, expiration) };
        let mut timers = timers.lock();
        let address = UnsafeRef::into_raw(node).addr();
        // SAFETY: callback cleanup and all references to this delivery have ended.
        // No access through the old node handle follows publication. A successor
        // can only be borrowed again by take_expired under this same owner lock.
        unsafe { timers.clear_running(address) };
        expired = timers.take_expired(due_at_ns);
    }
    any_due
}

#[cfg(unittest)]
unittest::path_mod_test!(tests, "../ut-tests/dispatch_unittest.rs");
