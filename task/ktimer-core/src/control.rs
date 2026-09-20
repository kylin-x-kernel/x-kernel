// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Callback-scoped operations, borrowing the current delivery's live storage.

use core::marker::PhantomData;

use kerrno::KResult;
use kspin::SpinNoIrq;
use ktime_types::MonotonicInstant;

use super::{cpu::CpuTimers, deadline::Deadline, index::TimerNode};

/// Non-owning access to the timer currently being delivered.
/// Cannot outlive the callback, cross threads, expose storage, or wait for itself.
/// Forgetting this value does not bypass dispatch's independent running cleanup.
pub struct TimerControl<'call> {
    timers: &'call SpinNoIrq<CpuTimers>,
    node: &'call TimerNode,
    _not_send_sync: PhantomData<*mut ()>,
}

impl<'call> TimerControl<'call> {
    /// # Safety
    /// node must be the active delivery in timers. Dispatch must keep node and
    /// handler live and running throughout 'call, and end every control borrow
    /// before clearing running. All deadline/tree operations use this owner lock.
    /// The external owner must also preserve storage for any successor armed
    /// through this control, until pending/running and other accesses are drained.
    pub(super) unsafe fn new(timers: &'call SpinNoIrq<CpuTimers>, node: &'call TimerNode) -> Self {
        Self {
            timers,
            node,
            _not_send_sync: PhantomData,
        }
    }

    /// Replaces the next setting on the current owner CPU without waiting.
    /// Installs one absolute deadline; returning without rearm does not create
    /// a successor or cancel a successor installed by another caller.
    /// Callback rearm and external starts serialize under the same owner lock;
    /// the last successful setting wins. Does not invoke a callback, allocate,
    /// or sleep. Business state belongs to the handler.
    ///
    /// # Errors
    /// Returns [`kerrno::KError::InvalidInput`] for deadlines above `u64::MAX`
    /// nanoseconds, leaving any accepted successor unchanged.
    ///
    /// # Panics
    /// With debug assertions enabled, an internal invariant check panics if this
    /// handle no longer belongs to the running delivery. Private construction
    /// and the callback lifetime prevent that condition in valid use.
    ///
    /// # Examples
    /// See the [callback rearm example](crate#examples).
    pub fn rearm(&self, deadline: MonotonicInstant) -> KResult<()> {
        let deadline = Deadline::new(deadline)?;
        let mut timers = self.timers.lock();
        debug_assert!(timers.is_running(self.node));
        // SAFETY: private construction ties this node to the live delivery
        // and this lock. Running prevents owner migration; the caller cannot
        // move/free the node or retain this control beyond callback completion.
        // Construction also requires external ownership through successor removal.
        unsafe { timers.arm(self.node, deadline) };
        Ok(())
    }
}
