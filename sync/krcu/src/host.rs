// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Host capabilities; RCU retains participation, demand and progress policy.

use core::marker::PhantomData;

use kcpu_id_map::LogicalCpuId;
use kerrno::KResult;
use kiface_task_api::TaskPreemptionIf;
use kiface_task_wait_api::{PreparedParking, TaskParkingIf};
use krcu_core::TaskReader;

pub use crate::service::PreparedGpTask;

/// A task-local borrow retained by the host, not permission to enter a read scope.
///
/// The provider owns the concrete current-task handle. RCU still checks context
/// and watching, manages nesting and decides when cleanup is permitted.
/// The adapter cannot lend its task-local borrow beyond its synchronous body:
///
/// ```compile_fail
/// let mut escaped = None;
/// krcu::host::RcuReaderStorageIf::with_current_reader(&mut |reader| escaped = Some(reader));
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

/// Launches the unique RCU grace-period driver in a task context.
#[kiface::interface]
pub trait RcuGpTaskLaunchIf {
    /// Transfers the prepared entry into one ordinary PID-less kernel task.
    ///
    /// Requires sleepable task context after scheduler startup. Run the entry
    /// exactly once on the new task, never inline on the caller's stack. The
    /// runtime supplies task name/stack and normal scheduling; RCU owns the
    /// driver and its startup acknowledgment. Allocation failure follows the
    /// existing fatal policy; do not discard the entry or add a recoverable
    /// failure after accepting its unique driver ownership.
    fn start_gp_task(entry: PreparedGpTask);
}

/// Borrows task-owned RCU reader storage without exporting the task implementation.
#[kiface::interface]
pub trait RcuReaderStorageIf {
    /// Invokes the body exactly once on this stack with retained current storage.
    ///
    /// Requires initialized current. Preserve IRQ/preemption state; do not take
    /// locks, allocate, increment an owner count or run scheduling checks. This
    /// includes NMI: the surrounding interrupt entry retains interrupted current.
    /// Task-context bodies may be involuntarily preempted or migrated. The body
    /// owns all RCU admission, nesting and cleanup decisions.
    fn with_current_reader(body_fn: &mut dyn FnMut(CurrentReader<'_>));
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
    TaskPreemptionIf::with_preempt_check_deferred(&mut || {
        result = Some(body_fn
            .take()
            .expect("RCU preemption provider invokes once")(
        ));
    });
    result.expect("RCU preemption provider must invoke the body")
}

/// Lends prepared completion waiting while keeping the result on this stack.
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: the static task provider calls its body \
              exactly once after successful preparation and never fails afterward"
)]
pub(crate) fn with_prepared_wait<R>(
    body_fn: impl FnOnce(&mut dyn kiface_task_wait_api::PreparedCompletionWait) -> R,
) -> KResult<R> {
    let mut body_fn = Some(body_fn);
    let mut result = None;
    kiface_task_wait_api::TaskCompletionWaitIf::with_prepared_wait(&mut |wait| {
        result = Some(body_fn.take().expect("RCU wait provider invokes once")(
            wait,
        ));
    })?;
    Ok(result.expect("RCU wait provider must invoke the body"))
}

/// Lends service parking while keeping the generic result on this stack.
#[expect(
    clippy::expect_used,
    reason = "CG-021-no-panic-on-reachable-failure: the static task provider calls its body \
              exactly once after successful preparation and never fails afterward"
)]
pub(crate) fn with_prepared_service_wait<R>(
    body_fn: impl FnOnce(&mut dyn PreparedParking) -> R,
) -> KResult<R> {
    let mut body_fn = Some(body_fn);
    let mut result = None;
    TaskParkingIf::with_prepared_parking(&mut |wait| {
        result = Some(body_fn
            .take()
            .expect("RCU service wait provider invokes once")(
            wait
        ));
    })?;
    Ok(result.expect("RCU service wait provider must invoke the body"))
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
    RcuReaderStorageIf::with_current_reader(&mut |reader| {
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
}
