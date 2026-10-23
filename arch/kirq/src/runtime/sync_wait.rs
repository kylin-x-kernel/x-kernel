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
    /// Reports whether the caller can perform a synchronous task wait now.
    ///
    /// IRQ teardown checks this before removing actions. The provider must
    /// reject absent/idle tasks, disabled IRQs, interrupt/BH-disabled context,
    /// held preemption protection and explicit RCU read scopes.
    fn can_wait() -> bool;

    /// Waits until the KIRQ teardown completion wake source is observed.
    ///
    /// The completion is only a wake source. Implementations should follow the
    /// scoped synchronous wait protocol, and callers must recheck their
    /// real predicate after this method returns.
    ///
    /// The caller must first check [`Self::can_wait`] and release all spinlocks
    /// before entering. Registration and parking do not allocate. The provider
    /// also validates direct calls before registering a waiter.
    ///
    /// # Errors
    /// Returns [`crate::IrqDescError::InvalidContext`] if the caller cannot sleep.
    /// This does not establish quiescence or authorize resource reclamation.
    fn wait_for_completion(completion: &kcompletion::Completion)
    -> Result<(), crate::IrqDescError>;
}
