// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! IRQ trap-entry adapter and architecture-specific compatibility helpers.

use kcpu::excp::{IRQ, NMI, register_trap_handler};

/// Dedicated NMI trap entry adapter.
///
/// Called by the architecture exception layer while handling an NMI-class trap.
/// The generic NMI path must remain valid with normal IRQs masked and must not
/// depend on sleepable context. Architecture entry supplies IRQ exclusion;
/// this adapter contributes only a preemption credit, not an IRQ mask.
///
/// # Safety
/// The caller must satisfy the architecture's [`NMI`] callback contract and
/// retain ordinary IRQ exclusion until return on this initialized CPU.
#[register_trap_handler(NMI)]
unsafe fn nmi_handler(vector: usize) -> bool {
    let guard = kspin::NoPreempt::new();
    // SAFETY: architecture entry supplies IRQ exclusion; guard excludes preemption.
    let handled = unsafe { kirq::handle_nmi(vector) };
    // Ordinary IRQ/exception exclusion still forbids scheduling at NMI return.
    let _ = guard;
    handled
}

/// Normal IRQ trap entry adapter.
///
/// Called by the architecture exception layer while handling a normal IRQ.
/// The caller must already be in interrupt-like
/// context with local IRQs masked; the handler may run IRQ callbacks and must
/// not be used as a sleepable task-context entry point.
///
/// # Safety
/// The caller must satisfy the architecture's [`IRQ`] callback contract. The
/// captured exception and stack must remain valid across the scheduling tail.
#[register_trap_handler(IRQ)]
unsafe fn irq_handler(vector: usize) -> bool {
    let guard = kspin::NoPreempt::new();
    // SAFETY: architecture entry supplies IRQ exclusion; guard pins dispatch.
    let handled = unsafe { kirq::handle_irq(vector) };

    // Release only this tail's preemption credit: ordinary guard release would
    // check cleanup twice and cannot schedule under the entry IRQ mask anyway.
    // Suspend the exception marker only for the completed-IRQ checkpoint, which
    // retains outer exclusions. If switched out, this stack and token resume
    // with the interrupted task.
    let suspended_exception = crate::context::suspend_active_exception_context();
    guard.release_without_check();
    kiface_task_api::TaskIrqExitCheckpointIf::check_preempt_pending();
    crate::context::resume_active_exception_context(suspended_exception);
    handled
}
