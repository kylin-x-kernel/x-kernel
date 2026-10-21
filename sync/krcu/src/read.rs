// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scoped participation for tasks and initialized interrupt handlers.

use krcu_core::ReadGuard;

use crate::context::current_reader;

/// Borrowed proof of participation in a task or interrupt kernel RCU read scope.
///
/// Only this crate constructs the token, after establishing host participation.
/// It borrows the internal guard, cannot be cloned or transferred to another
/// task, and performs no allocation or reference-count update. The scope owns
/// cleanup: forgetting a borrowed token does not suppress it.
///
/// A standalone core reader cannot manufacture host participation:
///
/// ```compile_fail
/// krcu_core::TaskReader::new().with_read(|guard| {
///     let _forged = krcu::ReadScope { _guard: guard };
/// });
/// ```
pub struct ReadScope<'reader> {
    _guard: &'reader ReadGuard<'reader>,
}

/// Runs a task-context read scope that may borrow several RCU containers.
///
/// Nested scopes are allowed. The closure may be involuntarily preempted or
/// migrated, but must not voluntarily sleep/yield, change watching state or
/// return holding an RCU bookkeeping lock. It may return copied or independently
/// owned results, not a scope-bound reference. Multiple reads are protected from
/// reclamation; they do not form an atomic snapshot across publications.
///
/// # Panics
/// Panics outside initialized watched task context, including exception,
/// IRQ/NMI and BH-disabled contexts, or if read nesting overflows.
///
/// A token cannot escape or be moved out to suppress internal cleanup:
///
/// ```compile_fail
/// let escaped = krcu::with_read(|scope| scope);
/// let _ = escaped;
/// ```
///
/// ```compile_fail
/// krcu::with_read(|scope| core::mem::forget(*scope));
/// ```
///
/// Even scoped threads cannot transfer the token to another execution context:
///
/// ```compile_fail
/// krcu::with_read(|token| {
///     std::thread::scope(|threads| {
///         threads.spawn(move || { let _token = token; });
///     });
/// });
/// ```
pub fn with_read<R>(read_fn: impl for<'scope> FnOnce(&'scope ReadScope<'_>) -> R) -> R {
    with_task_read(|guard| read_fn(&ReadScope { _guard: guard }))
}

/// Runs a current-task read scope without pinning its callback to a CPU.
///
/// The callback must not voluntarily block, change watching state, or return
/// holding an RCU bookkeeping lock. Ordinary exclusion guards may outlive the
/// callback; cleanup then waits for a safe boundary. With IPI support, an active
/// GP's timed pass kicks CPUs retaining demand, even after their CPU report.
/// No token borrow may escape. Permanent task exit uses terminal cleanup, not
/// this stack's destructor. This private entry rejects interrupt/BH contexts.
pub(super) fn with_task_read<R>(read_fn: impl for<'scope> FnOnce(&'scope ReadGuard<'_>) -> R) -> R {
    let task = {
        // Pin before selecting any per-CPU context slot: a permanent atomic
        // reference remains valid after migration but then names another CPU.
        // Reject NMI before touching task preemption accounting.
        let irq = kspin::IrqSave::new();
        assert!(!kirq::context::is_in_nmi());
        // SAFETY: irq retains ordinary IRQ exclusion through this snapshot.
        assert!(!unsafe { khal::context::in_exception_context_irqoff() });
        let task = ktask::current();
        assert!(krcu_core::RcuContextIf::current_cpu_watching().is_watching());
        assert!(!kirq::context::is_in_interrupt_context());
        // Context queries do not run scheduler/RCU checks on return. IRQ
        // exclusion alone pins admission; the read body remains preemptible.
        drop(irq);
        task
    };
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
/// BH-disabled task context alone is not a handler. This entry supports handlers
/// after the kernel's balanced interrupt entry has established watching, not
/// early architecture entry windows. Use the token with [`crate::Rcu::read`].
///
/// Normal entry/exit only accounts nesting; NMI takes no locks or guards. An
/// interrupt cannot create enrollment, so it leaves an interrupted task's
/// existing owner and cleanup notification for that task's continuation. Nesting
/// overflow/underflow remains a fatal invariant violation, not a recovery path.
///
/// # Panics
/// Panics outside watched hardirq, serving-softirq or NMI context, if current
/// is not initialized, or if nesting overflows.
///
/// The token cannot escape the handler's read scope:
///
/// ```compile_fail
/// let escaped = krcu::with_interrupt_read(|scope| scope);
/// let _ = escaped;
/// ```
pub fn with_interrupt_read<R>(read_fn: impl for<'scope> FnOnce(&'scope ReadScope<'_>) -> R) -> R {
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
    unsafe {
        current_reader(&task).with_read_deferred(|guard| read_fn(&ReadScope { _guard: guard }))
    }
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

unittest::path_mod_test!(tests, "../ut-tests/rcu_read.rs");
