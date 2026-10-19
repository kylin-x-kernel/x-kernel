// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System RCU state and execution-boundary policy.

use core::sync::atomic::{AtomicBool, Ordering};

use kcpu_id_map::LogicalCpuId;
use krcu_core::{CpuWatching, RcuContextIf, ReaderNode, TaskReader};

use crate::service;

pub(crate) static READER_NODES: [ReaderNode; kbuild_config::NR_CPUS] =
    [const { ReaderNode::new() }; kbuild_config::NR_CPUS];

struct CpuRcuState {
    watching: CpuWatching,
    // Notification only: nesting and enrollment remain the reader's truth.
    // Remote GP inspection never consumes local cleanup demand.
    cleanup_requested: AtomicBool,
}

impl CpuRcuState {
    const fn new() -> Self {
        Self {
            watching: CpuWatching::new(),
            cleanup_requested: AtomicBool::new(false),
        }
    }
}

// SAFETY: CpuWatching contains only atomic integer accounting, including its
// test-only depth. Together with AtomicBool these have no allocation, pointers
// or destructor. Copying the initial watched/clear template gives independent
// atomic objects in permanent CPU areas before any CPU accesses them.
unsafe impl kcpu_slot::StaticSlotValue for CpuRcuState {}

kcpu_slot::cpu_slot! {
    static CPU_STATE: crate::context::CpuRcuState = crate::context::CpuRcuState::new();
}

fn current_state() -> &'static CpuRcuState {
    // Like NMI context accounting, this can run before task/GP startup and
    // inside NMI. Atomic-only permanent storage needs no guard for a shared
    // borrow. Callers retain the existing CPU exclusion for state transitions.
    CPU_STATE.current_shared()
}

pub(crate) fn cpu_watching(area: &kcpu_slot::CpuArea) -> &CpuWatching {
    &CPU_STATE.at(area).watching
}

#[cfg(any(unittest, feature = "ipi"))]
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: GP participants completed CPU-area \
              publication before service startup; local callers run on initialized CPUs"
)]
pub(crate) fn needs_reader_cleanup(cpu: LogicalCpuId) -> bool {
    let area = kcpu_slot::area(cpu).expect("RCU observation requires a published CPU area");
    CPU_STATE
        .at(&area)
        .cleanup_requested
        .load(Ordering::Acquire)
}

fn needs_local_reader_cleanup() -> bool {
    current_state().cleanup_requested.load(Ordering::Acquire)
}

#[cfg(unittest)]
pub(crate) fn request_local_cleanup_for_tests() {
    current_state()
        .cleanup_requested
        .store(true, Ordering::Release);
}

pub(crate) fn current_reader(task: &ktask::CurrentTask) -> &TaskReader {
    // SAFETY: CurrentTask retains the current slot's borrow on this execution
    // stack. Our private callers only mutate it on this task or its strictly
    // nested handlers, under each core operation's additional context contract.
    unsafe { task.rcu_reader_for_integration() }
}

#[cfg(unittest)]
pub(crate) fn request_reader_cleanup(task: &ktask::CurrentTask) {
    let guard = kspin::NoPreemptIrqSave::new();
    let reader = current_reader(task);
    if !reader.is_reading() && reader.is_enrolled() {
        current_state()
            .cleanup_requested
            .store(true, Ordering::Release);
        ktask::request_current_reschedule();
    }
    // Preserve the caller's deferred exit: our temporary pin must not itself
    // trigger cleanup while restoring IRQ/preemption state. No IPI/controller
    // lock is acquired here; the active GP's timed pass delivers the kick.
    ktask::with_preempt_check_deferred(|| drop(guard));
}

/// Consumes local cleanup demand without completing a CPU observation.
///
/// # Safety
/// `reader` belongs to current with IRQs/preemption excluded, in watched non-NMI context
/// at a permitted cleanup boundary. Hold no reader-node lock. Its reader and
/// original node remain live; nested scopes must not change enrollment.
pub(crate) unsafe fn finish_reader_cleanup(reader: &TaskReader) {
    if reader.is_reading() {
        return;
    }
    // Clear before inspection/removal, never after: a later notification must
    // survive. An active scope above keeps both its enrollment and demand.
    current_state()
        .cleanup_requested
        .swap(false, Ordering::AcqRel);
    // SAFETY: caller supplies current, stable storage at a permitted excluded
    // boundary; the core rechecks nesting and removes only the original owner.
    unsafe { reader.finish_deferred_cleanup() };
}

#[kiface::provide]
impl RcuContextIf {
    fn current_cpu_watching() -> &'static CpuWatching {
        &current_state().watching
    }

    fn check_reader_cleanup() {
        check_reader_cleanup();
    }

    fn finish_reader_cleanup(context: krcu_core::ReaderCleanup<'_>) {
        // SAFETY: the caller retains the exclusions certified by construction
        // of this immediate-use context until this static call returns.
        unsafe { finish_reader_cleanup(context.reader()) };
    }

    fn note_context_switch(context: krcu_core::SchedulerObservation<'_>) {
        // SAFETY: the caller retains the outgoing task and scheduler exclusion
        // certified when constructing this immediate-use context.
        unsafe { note_context_switch(context.reader(), LogicalCpuId::new(context.cpu())) };
    }

    #[cfg(unittest)]
    fn reader_node_for_tests(cpu: usize) -> &'static ReaderNode {
        reader_node(LogicalCpuId::new(cpu))
    }
}

fn check_reader_cleanup() {
    if kirq::context::is_in_nmi() {
        return;
    }
    let task = ktask::current();
    let reader = current_reader(&task);
    if (reader.is_enrolled() || needs_local_reader_cleanup())
        && !reader.is_reading()
        && !ktask::current_preemption_disabled()
        && !khal::context::in_exception_context()
    {
        // Context queries release guards themselves; exclude preemption first
        // so those checks cannot recursively enter this cleanup checkpoint.
        let guard = kspin::NoPreemptIrqSave::new();
        if RcuContextIf::current_cpu_watching().is_watching()
            && !kirq::context::is_in_interrupt_context()
        {
            // SAFETY: retained current, watched task context, IRQ/preemption
            // excluded and no reader-node lock held. Nesting is rechecked.
            unsafe { finish_reader_cleanup(reader) };
        }
        // IRQ restoration may request work. The enclosing scheduler checkpoint
        // will inspect it; suppress only recursive checks during this release.
        ktask::with_preempt_check_deferred(|| drop(guard));
    }
}

/// Ends watched kernel execution at the final userspace-entry boundary.
///
/// # Safety
/// Call at the final IRQ-masked point of `UserContext::run`'s entry callback,
/// after accounting and all protected accesses. No RCU read scope or protected
/// borrow may span user execution. Until `exit_user_quiescent`, only user code,
/// audited RCU-free architecture transfer and balanced IRQ/NMI entries may run;
/// no kernel scheduling or migration may occur before that matching exit.
/// Controlled tests may model this callback pair without user execution while
/// retaining the same exclusion and access restrictions.
pub unsafe fn enter_user_quiescent() {
    // The architecture caller keeps IRQs masked through the final transfer,
    // not just cleanup. A local guard restoring IRQs here cannot replace that contract.
    debug_assert!(!karch::local_irq_enabled());
    {
        let task = ktask::current();
        let reader = current_reader(&task);
        // Avoid a preemption-count RMW on the ordinary unenrolled return path.
        // Debug context queries also need protection against guard-drop checks.
        if cfg!(debug_assertions) || reader.is_enrolled() || needs_local_reader_cleanup() {
            // Do not let cleanup's guard release schedule in the architecture's
            // final user-entry callback. Its caller already masks ordinary IRQs.
            ktask::with_preempt_check_deferred(|| {
                debug_assert!(!reader.is_reading());
                debug_assert!(!kirq::context::is_in_interrupt_context());
                // SAFETY: retained current task, watched task context, IRQs masked
                // and preemption excluded; no node lock is held at this boundary.
                unsafe { finish_reader_cleanup(reader) };
            });
        }
    }
    let watching = RcuContextIf::current_cpu_watching();
    // SAFETY: IRQ exclusion and the nonscheduling transition pin CPU selection.
    // Its caller finished protected work and limits the unwatched window to
    // user execution and audited transfer code, with balanced interrupt entries.
    unsafe { watching.enter_quiescent() };
}

/// Restores watching on user return, before accounting or trap dispatch.
///
/// # Safety
/// Match `enter_user_quiescent` on the same CPU in `UserContext::run`'s first
/// IRQ-masked return callback. No protected access, scheduling or ordinary
/// trap dispatch may precede this call. Nested interrupt entries must have
/// completed; keep IRQs masked until this transition has finished.
/// Controlled tests must uphold the same matching and exclusion requirements.
pub unsafe fn exit_user_quiescent() {
    debug_assert!(!karch::local_irq_enabled());
    let watching = RcuContextIf::current_cpu_watching();
    // SAFETY: The architecture restored kernel context without scheduling and
    // invokes this callback before trap dispatch, on the original masked CPU.
    unsafe { watching.exit_quiescent() };
}

/// Called before the same-task decision and by the migration helper's path.
///
/// # Safety
/// `reader` must belong to the current, Arc-owned task on `cpu`, with ordinary IRQs and
/// preemption excluded. Its storage must follow the task exit/GC protocol.
/// This must not run in NMI or while holding a reader node/coordinator lock.
unsafe fn note_context_switch(reader: &TaskReader, cpu: LogicalCpuId) {
    // SAFETY: the outgoing current task is retained and IRQs/preemption are
    // excluded. No node lock is held here; nested NMI accounting cannot unlink.
    // Drain finished scopes before reporting this CPU's scheduler boundary.
    unsafe { finish_reader_cleanup(reader) };
    if let Some(callbacks) = service::callbacks() {
        // SAFETY: this is the outgoing current task under the caller's IRQ and
        // preemption exclusion. Task retirement retains its embedded reader;
        // the service reports only after enrollment and does not wake inline.
        unsafe { callbacks.note_context_switch(cpu.as_usize(), reader) };
        return;
    }
    // SAFETY: switch_to runs with IRQs/preemption excluded for its current task.
    // Static nodes never move; the task remains Arc-owned while runnable, and
    // finish_task_switch unlinks exited readers before releasing the old owner.
    unsafe { READER_NODES[cpu.as_usize()].enroll(reader) };
}

#[cfg(unittest)]
pub(crate) fn reader_node(cpu: LogicalCpuId) -> &'static ReaderNode {
    &READER_NODES[cpu.as_usize()]
}

unittest::path_mod_test!(tests_service_startup, "../ut-tests/rcu_service_startup.rs");
unittest::path_mod_test!(tests_user_boundary, "../ut-tests/rcu_user_boundary.rs");
unittest::path_mod_test!(tests_cleanup, "../ut-tests/rcu_cleanup.rs");
unittest::path_mod_test!(tests_cpu_state, "../ut-tests/rcu_cpu_state.rs");
#[cfg(feature = "smp")]
unittest::path_mod_test!(tests_implicit, "../ut-tests/rcu_implicit.rs");
