// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Low-level task execution, preemption, blocking-context and diagnostic contracts.
//!
//! Consumers depend on these contracts without depending on a scheduler.
//! The linked task runtime supplies exactly one provider for each interface.
//! This crate owns no scheduler, lock algorithm, or global runtime state.

#![no_std]

/// Current-task blocking admission, without preparing or performing a wait.
#[kiface::interface]
pub trait TaskBlockingContextIf {
    /// Samples whether the current task may block now.
    ///
    /// Call outside NMI with initialized CPU context. Returns false for absent
    /// or idle current, masked IRQs, interrupt/BH context, tracked preemption
    /// exclusion or an active reader scope. Never allocates, schedules, wakes,
    /// runs cleanup checks or consumes pending requests. This is only a snapshot:
    /// it does not detect dependency cycles or certify caller-held mutexes.
    fn is_sleepable_context() -> bool;
}

/// Nested preemption exclusion owned by the current task runtime.
///
/// These operations do not change IRQ masks. The three credit acquire/release
/// methods are no-ops before current installation; a credit must not span it.
/// Other methods state their own initialization requirements.
/// Once initialized, releases must balance acquisitions on the same task.
/// This is not an NMI synchronization mechanism.
#[kiface::interface]
pub trait TaskPreemptionIf {
    /// Reports a pending scheduling request for the current task.
    ///
    /// Called with local IRQs disabled and the CPU pinned. This query must not
    /// schedule, block, or consume the request. Return false before task setup
    /// regardless of whether forced preemption is configured.
    fn needs_resched() -> bool;

    /// Reads current's actual preemption-disable count, not its IRQ mask.
    ///
    /// Requires initialized current; never schedules or changes exclusion.
    fn current_preemption_disabled() -> bool;

    /// Sets current's reschedule request without checking it or notifying a CPU.
    ///
    /// Requires initialized current outside NMI. A later normal scheduling
    /// checkpoint must consume the request; Callers own progress notification.
    fn request_current_reschedule();

    /// Runs the ordinary task checkpoint, including deferred reader cleanup.
    ///
    /// Requires initialized current. May schedule when the host's IRQ, exception
    /// and preemption checks permit.
    /// Call outside NMI with no RCU bookkeeping lock held. Callers choose when to request a checkpoint, not whether a task switches.
    fn check_preempt_pending();

    /// Invokes one synchronous body with an extra preemption-disable credit.
    ///
    /// Requires initialized current outside NMI. Preserve IRQ state and remove
    /// only this credit on return or unwind, without running a scheduling or
    /// cleanup check. The body must not block or consume this credit; callers
    /// arrange a later normal checkpoint, including on unwind. Do not allocate.
    fn with_preempt_check_deferred(body_fn: &mut dyn FnMut());

    /// Adds one preemption exclusion credit without scheduling.
    fn disable_preempt();

    /// Releases one credit and performs the runtime's pending-work check.
    ///
    /// The check may schedule when the surrounding context permits it.
    /// Combined IRQ/preemption guards must restore IRQ state before this call.
    fn enable_preempt();

    /// Releases one credit without checking pending work or scheduling.
    /// The caller must provide the subsequent explicit scheduling checkpoint.
    fn enable_preempt_without_check();
}

/// Task identity and synchronous scheduling services.
///
/// Calls require an installed current task and are not interrupt/NMI services.
/// Identity queries may run with IRQs masked or preemption disabled; yielding
/// and waiting require sleepable task context, without spinlocks, IRQ masking,
/// preemption protection, or other scopes that prohibit scheduling.
/// Callers establish these conditions; the interface does not validate them.
#[kiface::interface]
pub trait TaskExecutionIf {
    /// Returns the current task's nonzero, stable, unique ownership key.
    ///
    /// Keys must survive yielding, waiting and CPU migration, must not be shared
    /// by distinct live tasks, and must not be reused while a consumer still
    /// records that owner. Zero is reserved for an absent owner. The provider
    /// must reject an absent current task rather than invent an identity.
    /// This query must not block, unwind on valid input, or reenter its caller.
    fn current_owner_key() -> core::num::NonZeroU64;

    /// Yields in sleepable task context, resuming this same task, possibly on
    /// another CPU. Must not unwind on valid input.
    fn yield_now();

    /// Drives a borrowed pinned future to completion on the current task.
    ///
    /// Requires sleepable task context. The provider must use the future's
    /// waker without losing wakeups and return only after `Ready(())`.
    /// The future need not be Send; it remains pinned in caller-owned storage
    /// and may not escape this synchronous call or move to another task.
    /// The provider must not unwind on valid input; progress requires the
    /// future's producer and scheduler to make progress.
    fn block_on(future: core::pin::Pin<&mut dyn core::future::Future<Output = ()>>);
}

/// Current-task lock diagnostics, independent of any particular lock algorithm.
///
/// Calls require an installed current task, not interrupt/NMI context. They
/// may run with IRQs masked or preemption disabled and must not block, unwind,
/// or reenter the calling lock. Addresses are opaque identities, never pointers
/// to dereference. Providers own clocks and task bookkeeping, not exclusion.
/// Methods are always declared; consumers may gate calls by feature, and a
/// provider without diagnostic storage may implement them as no-ops.
#[kiface::interface]
pub trait TaskLockDiagnosticsIf {
    /// Clears waiting diagnostics and records a lock held by this task.
    /// Called after successful blocking or nonblocking acquisition.
    fn lock_acquired(address: usize);

    /// Records the lock this task is about to wait for using host-owned time.
    /// Repeated waits may update the same diagnostic.
    fn lock_waiting(address: usize);

    /// Removes a held-lock diagnostic after release, before waiter notification.
    /// The address must match the corresponding acquisition on this task.
    fn lock_released(address: usize);
}

/// Scheduler checkpoint after normal IRQ dispatch and controller completion.
///
/// The trap adapter calls this with IRQs masked, its preemption guard released
/// and the architecture exception marker suspended. The provider must retain
/// outer preemption, interrupt-context and CPU-watching exclusions. Dedicated
/// NMI entry must not use this checkpoint.
#[kiface::interface]
pub trait TaskIrqExitCheckpointIf {
    /// Checks pending scheduling work without enabling local IRQs on return.
    fn check_preempt_pending();
}

/// Statically linked scheduler endpoint supplied by the task subsystem.
#[kiface::interface]
pub trait TaskRescheduleRequestIf {
    /// Marks a local reschedule request, or does nothing on a UP runtime.
    ///
    /// Called with local IRQs disabled after scheduler initialization, either
    /// during boot drain or from the IPI handler. Must not allocate or sleep.
    fn on_reschedule();
}
