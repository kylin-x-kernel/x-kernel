// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host capabilities; RCU retains participation, demand and progress policy.

use core::{marker::PhantomData, task::Waker};

use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use krcu_core::TaskReader;

pub use crate::service::PreparedGpTask;

/// CPU and execution-context facts, not permission to enter a read scope or unlink it.
///
/// Requires initialized CPU slots. Queries preserve IRQ/preemption state and
/// never allocate, schedule, or run cleanup checks. Results describe the call,
/// not a lasting exclusion token; callers retain protection across dependent
/// operations. RCU owns watching, nesting and admission decisions.
#[kiface::interface]
pub trait RcuExecutionIf {
    /// Samples the current logical CPU without pinning subsequent operations.
    ///
    /// Never allocates, locks or checks scheduling. Callers keep their own
    /// exclusion across CPU-local queue operations that use this identity.
    fn current_cpu() -> LogicalCpuId;

    /// Samples exception presence under temporary IRQ exclusion, then restores it.
    ///
    /// Do not dereference the trapframe or run a scheduling/cleanup check.
    fn is_in_exception_context() -> bool;

    /// Lends exception presence to one synchronous body while ordinary IRQs stay masked.
    ///
    /// Pin before sampling the per-CPU exception marker and retain that same
    /// exclusion through the body. Restore the caller's mask on return or unwind,
    /// without changing preemption counts or running scheduling/cleanup checks.
    /// Do not allocate, lock or access current just to supply the snapshot.
    /// The body must not enable IRQs, block or migrate; it owns admission checks.
    fn with_exception_snapshot(body_fn: &mut dyn FnMut(bool));

    /// Reports effective NMI nesting without locks, guards or task access.
    fn is_in_nmi() -> bool;

    /// Reports NMI, hardirq, serving softirq, or task-context BH exclusion.
    ///
    /// Test NMI before any ordinary context lock. Outside NMI, protect the
    /// per-CPU snapshot without running a scheduler check on guard release.
    fn is_in_interrupt_context() -> bool;

    /// Reports an executing NMI, hardirq or softirq handler, excluding BH-only tasks.
    ///
    /// In NMI this must return without locks, guards or task access. Outside
    /// NMI, preserve the same snapshot protection as is_in_interrupt_context.
    fn is_in_interrupt_handler() -> bool;
}

/// A task-local borrow retained by the host, not permission to enter a read scope.
///
/// The provider owns the concrete current-task handle. RCU still checks context
/// and watching, manages nesting and decides when cleanup is permitted.
/// The adapter cannot lend its task-local borrow beyond its synchronous body:
///
/// ```compile_fail
/// let mut escaped = None;
/// krcu::host::RcuTaskIf::with_current_reader(&mut |reader| escaped = Some(reader));
/// ```
pub struct CurrentReader<'task> {
    reader: &'task TaskReader,
    _local: PhantomData<*mut ()>,
}

impl<'task> CurrentReader<'task> {
    /// Certifies the host's retained current-task borrow for one synchronous call.
    ///
    /// # Safety
    /// `reader` must belong to the current task. Retain its storage throughout
    /// this borrow, including involuntary switches/migration and strictly nested
    /// interrupts. Do not transfer the borrow to another task or let it outlive
    /// that execution stack. This certifies storage and identity only, not
    /// watching, scope admission or permission to unlink enrollment.
    pub unsafe fn new(reader: &'task TaskReader) -> Self {
        Self {
            reader,
            _local: PhantomData,
        }
    }

    pub(crate) fn reader(&self) -> &TaskReader {
        self.reader
    }
}

/// Prepared task-bound parking; wakeups are hints, never RCU completion.
///
/// The host retains this object on the preparing task's stack. It must remember
/// a wake delivered before park and permit spurious returns. Neither method
/// registers a wait or allocates; all fallible preparation precedes the borrow.
/// The borrow is not Send/Sync and must not escape the synchronous adapter body.
pub trait TaskWait {
    /// Clones the prepared waker without allocating. It may outlive this borrow.
    fn waker(&self) -> Waker;

    /// Parks this same task without returning an interruption error.
    ///
    /// Requires sleepable context and no reader/service lock. Must not consume
    /// pending signals; the caller rechecks its completion predicate on return.
    fn park(&mut self);
}

/// Borrowed one-shot timer; RCU owns the deadline and the meaning of expiry.
///
/// The host keeps its allocation alive for the whole adapter body. Rearming
/// reuses that allocation and never invokes the expiry callback inline.
pub trait ProgressTimer {
    /// Starts or replaces a deadline in monotonic nanoseconds since boot.
    ///
    /// # Errors
    /// Reports invalid deadlines or CPU routing without changing the old timer.
    ///
    /// # Safety
    /// The current CPU's scheduler and timer IRQ service must be initialized.
    /// The supplied expiry callback must not sleep, unwind, enable IRQs, reenter
    /// timer dispatch or synchronously cancel this timer.
    unsafe fn arm(&mut self, deadline_ns: u64) -> KResult<()>;

    /// Removes pending expiry and waits for any running callback to return.
    ///
    /// Hold no lock needed by the callback and do not prevent its CPU's progress.
    /// Never call from the expiry callback itself.
    fn cancel_sync(&mut self);
}

/// Supplies timer mechanics, not progress intervals or quiescent observations.
#[kiface::interface]
pub trait RcuTimerIf {
    /// Samples the same monotonic nanosecond clock used by timer deadlines.
    fn monotonic_time_nanos() -> u64;

    /// Lends an initially inactive timer to exactly one synchronous body.
    ///
    /// Requires initialized per-CPU storage. Preparation follows the kernel's
    /// allocation policy; no extra allocation is needed solely for type erasure.
    /// The body may park or migrate. Expiry invokes notify_fn from timer IRQ
    /// context, and the host drains delivery before releasing storage on return.
    /// Before returning, release callback-needed locks and leave its CPU able
    /// to progress; implicit draining has the same constraints as cancel_sync.
    /// The timer borrow cannot escape this body.
    fn with_progress_timer(notify_fn: fn(), body_fn: &mut dyn FnMut(&mut dyn ProgressTimer));
}

/// Supplies task storage, execution controls and parking without owning RCU policy.
#[kiface::interface]
pub trait RcuTaskIf {
    /// Samples whether current may enter a blocking wait, without preparing one.
    ///
    /// Call outside NMI with initialized CPU context, before adding temporary
    /// IRQ/preemption exclusion. Return false for absent/idle current, masked
    /// IRQs, interrupt/BH context, tracked preemption exclusion or an active
    /// reader scope. Preserve exclusion and pending requests; never allocate,
    /// schedule, wake or run cleanup checks. RCU separately checks exception
    /// presence and watching. This snapshot does not detect dependency cycles
    /// or certify caller-held sleepable locks.
    fn is_sleepable_context() -> bool;

    /// Reads current's actual preemption-disable count, not its IRQ mask.
    ///
    /// Requires initialized current; never schedules or changes exclusion.
    fn current_preemption_disabled() -> bool;

    /// Sets current's reschedule request without checking it or notifying a CPU.
    ///
    /// Requires initialized current outside NMI. A later normal scheduling
    /// checkpoint must consume the request; RCU owns progress notification.
    fn request_current_reschedule();

    /// Runs the ordinary task checkpoint, including deferred reader cleanup.
    ///
    /// May schedule when the host's IRQ, exception and preemption checks permit.
    /// Call outside NMI with no RCU bookkeeping lock held. RCU decides when
    /// its read-scope exit requests this checkpoint, not whether a task switches.
    fn check_preempt_pending();

    /// Invokes one synchronous body with an extra preemption-disable credit.
    ///
    /// Requires initialized current outside NMI. Preserve IRQ state and remove
    /// only this credit on return or unwind, without running a scheduling or
    /// cleanup check. The body must not block or consume this credit; callers
    /// arrange a later normal checkpoint, including on unwind. Do not allocate.
    fn with_preempt_check_deferred(body_fn: &mut dyn FnMut());

    /// Transfers the prepared entry into one ordinary PID-less kernel task.
    ///
    /// Requires sleepable task context after scheduler startup. Run the entry
    /// exactly once on the new task, never inline on the caller's stack. The
    /// runtime supplies task name/stack and normal scheduling; RCU owns the
    /// driver and its startup acknowledgment. Allocation failure follows the
    /// existing fatal policy; do not discard the entry or add a recoverable
    /// failure after accepting its unique driver ownership.
    fn start_gp_task(entry: PreparedGpTask);

    /// Invokes the body exactly once on this stack with retained current storage.
    ///
    /// Requires initialized current. Preserve IRQ/preemption state; do not take
    /// locks, allocate, increment an owner count or run scheduling checks. This
    /// includes NMI: the surrounding interrupt entry retains interrupted current.
    /// Task-context bodies may be involuntarily preempted or migrated. The body
    /// owns all RCU admission, nesting and cleanup decisions.
    fn with_current_reader(body_fn: &mut dyn FnMut(CurrentReader<'_>));

    /// Prepares parking resources, then lends them to the body exactly once.
    ///
    /// On failure do not invoke the body; after invoking it return only Ok.
    /// Retain the concrete wait on this stack through the body, without an
    /// additional allocation just to erase its type. The body may sleep on the
    /// preparing task and owns all completion checks and wait registrations.
    ///
    /// # Errors
    /// Rejects contexts where the current task cannot sleep before invoking
    /// the body. Existing host allocations retain their fatal OOM policy.
    ///
    /// The host's task-bound wait cannot escape the body:
    ///
    /// ```compile_fail
    /// let mut escaped = None;
    /// let _ = krcu::host::RcuTaskIf::with_prepared_wait(&mut |wait| {
    ///     escaped = Some(wait);
    /// });
    /// ```
    fn with_prepared_wait(body_fn: &mut dyn FnMut(&mut dyn TaskWait)) -> KResult<()>;

    /// Holds the permanent kernel RCU drain mutex across one synchronous body.
    ///
    /// Invoke the body exactly once after acquiring a sleepable mutex shared
    /// by all callers, and release it only after the body returns. The body may
    /// park; it owns marker installation and completion. Requires sleepable
    /// task context, no recursive call and no lock needed by callbacks/service.
    /// The provider owns no markers or completion counters.
    fn with_serialized_drain(body_fn: &mut dyn FnMut());
}

/// Defers checks around a once-only body that may consume an exclusion guard.
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: the static task provider must invoke its \
              synchronous body exactly once under deferred preemption checking"
)]
pub(crate) fn with_preempt_check_deferred<R>(body_fn: impl FnOnce() -> R) -> R {
    let mut body_fn = Some(body_fn);
    let mut result = None;
    RcuTaskIf::with_preempt_check_deferred(&mut || {
        result = Some(body_fn
            .take()
            .expect("RCU preemption provider invokes once")(
        ));
    });
    result.expect("RCU preemption provider must invoke the body")
}

/// Lends prepared host parking while keeping the generic result on this stack.
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: the static task provider calls its body \
              exactly once after successful preparation and never fails afterward"
)]
pub(crate) fn with_prepared_wait<R>(body_fn: impl FnOnce(&mut dyn TaskWait) -> R) -> KResult<R> {
    let mut body_fn = Some(body_fn);
    let mut result = None;
    RcuTaskIf::with_prepared_wait(&mut |wait| {
        result = Some(body_fn.take().expect("RCU wait provider invokes once")(
            wait,
        ));
    })?;
    Ok(result.expect("RCU wait provider must invoke the body"))
}

/// Keeps the generic return value on the caller's stack across the static adapter.
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: the statically linked task provider must \
              invoke the synchronous body exactly once"
)]
pub(crate) fn with_current_reader<R>(body_fn: impl FnOnce(CurrentReader<'_>) -> R) -> R {
    let mut body_fn = Some(body_fn);
    let mut result = None;
    RcuTaskIf::with_current_reader(&mut |reader| {
        result = Some(body_fn.take().expect("RCU task provider invokes once")(
            reader,
        ));
    });
    result.expect("RCU task provider must invoke the body")
}

/// Fixed RCU softirq carrier; callback policy and queue state remain in RCU.
#[kiface::interface]
pub trait RcuSoftirqIf {
    /// Installs the action without allocating, invoking it or raising pending work.
    ///
    /// Return false on conflict without changing the existing action. The caller
    /// serializes registration with its sole driver claim. Do not sleep or
    /// reenter startup; registration runs under the caller's startup lock.
    /// After success dispatch must exclude migration and run the action in
    /// non-sleepable softirq context, with no carrier lock held around the call.
    /// The action must be ready before installation and must not block or unwind.
    fn install(action_fn: fn()) -> bool;

    /// Marks this CPU's RCU softirq pending without invoking the action inline.
    ///
    /// The caller excludes migration through its queue check and this request.
    /// Requires initialized local softirq service, outside NMI, with no RCU or
    /// scheduler lock held: raising may wake the daemon. The common runner owns
    /// restart/time limits; only RCU decides whether ready callbacks remain.
    fn raise_local();
}

/// Statically supplied by runtime integration, not a consumer RCU interface.
///
/// Notifications are hints, never CPU observations or proof of GP completion.
/// The provider must not access reader nodes, queues or completion accounting.
#[kiface::interface]
pub trait RcuNotifyIf {
    /// Requests RCU softirq processing on an initialized participating CPU.
    ///
    /// Demand/work must be published before this call. With IPI support the
    /// provider must defer delivery without waking a task inline. Without IPI
    /// support only the current CPU is supported, and raising softirq may wake
    /// its daemon: callers must hold no scheduler or RCU bookkeeping lock.
    /// Never allocate, wait, invoke RCU callbacks inline, or call from NMI.
    ///
    /// # Errors
    /// Returns an error when the target notification transport is unavailable.
    fn notify_rcu(cpu: LogicalCpuId) -> KResult<()>;

    /// Requests a scheduling opportunity without reporting quiescence itself.
    ///
    /// Requires initialized IPI delivery. Must not allocate, wait or wake a task
    /// inline. RCU retains the decision whether a CPU needs this notification.
    ///
    /// # Errors
    /// Returns an error when the target notification transport is unavailable.
    #[cfg(feature = "ipi")]
    fn notify_reschedule(cpu: LogicalCpuId) -> KResult<()>;
}
