// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ trap-entry adapter and architecture-specific compatibility helpers.

use kcpu::excp::{IRQ, NMI, register_trap_handler};

/// Pseudo-NMI trap entry adapter.
///
/// Called by the architecture exception layer while handling an NMI-class trap.
/// The generic NMI path must remain valid with normal IRQs masked and must not
/// depend on sleepable context.
#[register_trap_handler(NMI)]
pub fn nmi_handler(vector: usize) -> bool {
    let guard = kspin::NoPreempt::new();
    let handled = kirq::handle_nmi(vector);
    let _ = guard; // rescheduling may occur when preemption is re-enabled.
    handled
}

/// Normal IRQ trap entry adapter.
///
/// Called by the architecture exception layer while handling a normal IRQ or
/// hypervisor VM-exit interrupt. The caller must already be in interrupt-like
/// context with local IRQs masked; the handler may run IRQ callbacks and must
/// not be used as a sleepable task-context entry point.
#[register_trap_handler(IRQ)]
pub fn irq_handler(vector: usize) -> bool {
    let guard = kspin::NoPreempt::new();
    let handled = kirq::handle_irq(vector);

    // The architecture's EL1 exception guard is still active here. Dropping
    // `NoPreempt` with that marker installed makes ktask reject a pending
    // reschedule, and there is no second check after the guard later unwinds.
    // Temporarily suspend the marker after the IRQ is completed so the normal
    // enable-preempt hook can switch in this IRQ-tail safe point. If a switch
    // occurs, this stack (and token) resume with the interrupted task.
    let suspended_exception = crate::context::suspend_active_exception_context();
    drop(guard);
    crate::context::resume_active_exception_context(suspended_exception);
    handled
}
