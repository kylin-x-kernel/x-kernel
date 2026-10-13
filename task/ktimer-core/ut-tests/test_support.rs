// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scoped node storage for upper-layer timer integration tests.

use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use ktime_types::MonotonicInstant;

#[cfg(feature = "smp")]
pub use crate::set::tests::{concurrent_cancel_waits_for_cleanup, concurrent_owner_routes};
use crate::{
    TimerSet,
    control::TimerControl,
    deadline::{Deadline, Expiration},
    index::TimerNode,
};

/// Arms a borrowed test timer, exercises the platform, then synchronously drains it.
/// Node and callback stay private and fixed on this stack throughout the exercise.
/// No hardware refresh is performed; the platform exercise must request it.
///
/// # Errors
/// Returns [`kerrno::KError::InvalidInput`] for an out-of-range CPU, a deadline
/// above `u64::MAX` nanoseconds. The exercise is
/// not called on error.
///
/// # Safety
/// Keep the set stable and service only initialized CPUs under [`TimerSet::dispatch_due`]'s
/// context contract. callback must suit hard IRQ, not unwind or block; exercise
/// must not unwind and must allow any in-flight callback to finish before return.
/// Do not invoke this helper from timer delivery on the selected CPU.
pub unsafe fn with_armed_timer<const N: usize>(
    timers: &TimerSet<N>,
    cpu: LogicalCpuId,
    deadline: MonotonicInstant,
    callback: impl Fn() + Sync,
    exercise: impl FnOnce(),
) -> KResult<()> {
    let deadline = Deadline::new(deadline)?;
    let handler = |_: TimerControl<'_>, _: Expiration| callback();
    // SAFETY: handler and its captures remain stack-stable until cancellation
    // below has drained delivery; no reference to the node escapes this scope.
    let node = unsafe { TimerNode::with_handler(deadline, &handler) };
    // SAFETY: node and handler outlive this one set's pending/running accesses;
    // the caller provides an initialized CPU and valid platform dispatch.
    unsafe { timers.arm_on(&node, cpu, deadline) }?;
    exercise();
    // SAFETY: no external rearm or node reference escaped. The exercise allows
    // delivery to finish, and callback does not wait for this cancellation.
    unsafe { timers.cancel_sync(&node) };
    Ok(())
}
