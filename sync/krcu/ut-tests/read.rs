// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Private scope policies for controlled task and interrupt consumers.

use krcu_core::ReadGuard;

use crate::context::current_reader;

/// Runs a current-task read scope without pinning its callback to a CPU.
///
/// The callback must not voluntarily block, change watching state, or return
/// holding an RCU bookkeeping lock. Ordinary exclusion guards may outlive the
/// callback; cleanup then waits for a safe boundary. With IPI support, an active
/// GP's timed pass kicks CPUs retaining demand, even after their CPU report.
/// No token borrow may escape. Permanent task exit uses terminal cleanup, not
/// this stack's destructor. This private entry rejects interrupt/BH contexts.
pub(super) fn with_task_read<R>(read_fn: impl for<'scope> FnOnce(&'scope ReadGuard<'_>) -> R) -> R {
    assert!(!kirq::context::is_in_nmi());
    assert!(!khal::context::in_exception_context());
    let task = ktask::current();
    {
        // Context queries release their own guards. Keep an outer credit so
        // those releases cannot recursively enter the preemption probe.
        let _guard = kspin::NoPreemptIrqSave::new();
        assert!(krcu_core::RcuContextIf::current_cpu_watching().is_watching());
        assert!(!kirq::context::is_in_interrupt_context());
    }
    let _exit = TaskReadExit { task: &task };
    // SAFETY: current's storage remains live across involuntary switches and
    // migration. Only this task and strictly nested interrupt scopes use it.
    // The callback follows the task-scope restrictions and _exit runs after the
    // core guard, retaining enrollment or arranging safe-context cleanup.
    unsafe { current_reader(&task).with_read_deferred(read_fn) }
}

/// Runs a strictly nested scope inside an already-watched IRQ/NMI handler.
///
/// The callback must return before that handler exits, without blocking,
/// scheduling, migration, changing watching or enrollment, or abandoning its
/// stack. Current must already be initialized and retained by interrupt entry.
/// BH-disabled task context alone is not a handler. This unittest-only entry
/// does not authorize unaudited architecture entry windows or public consumers.
///
/// Normal entry/exit only accounts nesting; NMI takes no locks or guards. An
/// interrupt cannot create enrollment, so it leaves an interrupted task's
/// existing owner and cleanup notification for that task's continuation. Nesting
/// overflow/underflow remains a fatal invariant violation, not a recovery path.
pub(super) fn with_interrupt_read<R>(
    read_fn: impl for<'scope> FnOnce(&'scope ReadGuard<'_>) -> R,
) -> R {
    // This query checks the lock-free NMI counter first, before considering
    // ordinary context locks. Never replace it with an unconditional snapshot.
    assert!(matches!(
        kirq::context::interrupt_context_level(),
        kirq::context::InterruptContextLevel::Nmi
            | kirq::context::InterruptContextLevel::Hardirq
            | kirq::context::InterruptContextLevel::Softirq
    ));
    assert!(krcu_core::RcuContextIf::current_cpu_watching().is_watching());
    let task = ktask::current();
    // SAFETY: the surrounding handler excludes scheduling/migration and retains
    // current. Strictly nested scopes finish on this stack and never alter
    // enrollment. The interrupted task's normal/terminal exit retains its
    // cleanup responsibility; this path must not run TaskReadExit in NMI.
    unsafe { current_reader(&task).with_read_deferred(read_fn) }
}

struct TaskReadExit<'task> {
    task: &'task ktask::CurrentTask,
}

impl Drop for TaskReadExit<'_> {
    fn drop(&mut self) {
        let reader = current_reader(self.task);
        if reader.is_reading() || !reader.is_enrolled() {
            return;
        }
        if !karch::local_irq_enabled() || ktask::current_preemption_disabled() {
            // Like Linux's non-expedited special unlock, retain the membership
            // and request a later scheduler evaluation. Never clear an existing
            // request or wake the scheduler while the caller holds exclusion.
            crate::context::request_reader_cleanup(self.task);
            return;
        }
        // Reuse the guarded context checks and recursion suppression. This
        // drains even without need_resched, but does not invent a CPU report.
        ktask::check_preempt_pending();
    }
}

unittest::path_mod_test!(tests, "rcu_read.rs");
