// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scheduler-facing wait bridge for IRQ synchronization.

/// Wait bridge used by KIRQ teardown synchronization.
///
/// `kirq` owns the IRQ lifecycle predicate, but it does not own task blocking.
/// The scheduler layer provides this interface so `synchronize_irq()` and
/// `free_irq()` can sleep without adding a `kirq -> ktask` dependency.
#[kiface::interface]
pub trait IrqSyncWaitIf {
    /// Waits until the KIRQ teardown completion wake source is observed.
    ///
    /// The completion is only a wake source. Implementations should follow the
    /// `try_wait/register/try_wait` protocol, and callers must recheck their
    /// real predicate after this method returns.
    ///
    /// # Errors
    ///
    /// Returns [`kpoll::PollRegisterError::InvalidState`] when no sleepable
    /// current task context is available, or a registration error if the wait
    /// source cannot record the current waiter.
    fn wait_for_completion(completion: &kpoll::Completion) -> Result<(), kpoll::PollRegisterError>;
}
