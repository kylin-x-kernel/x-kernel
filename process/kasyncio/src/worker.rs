// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Async worker lifecycle: lazy spawn, idle park, and shutdown-driven exit.
//!
//! Workers are **sibling threads of the owning process** created through the
//! ordinary clone path ([`kprocess::Thread::prepare_thread_clone`] →
//! [`ktask::TaskInner::new_user`] → [`kprocess::publish_user_task`]) with a
//! pure kernel-mode entry loop. This is what makes user-buffer operations
//! sound: a worker dereferences user VAs through the same guarded
//! user-memory path as a syscall, resolves against its own process's address
//! space, and shares the process fd table and credentials — without any
//! mm-adoption machinery.
//!
//! # Exit contract
//!
//! Workers participate in process thread membership, so the process's
//! address space and fd table stay alive for as long as any worker runs.
//! Retirement happens when any of the following holds; the loop then calls
//! [`posix_process::do_exit_worker`] so membership, cgroup charge, and — when
//! the worker is the last member — the process tail are handled by the ordinary
//! exit path without publishing a worker status:
//!
//! - The context shut down. Operations already dispatched run to their
//!   natural completion; operations still queued (never dispatched) were
//!   cancelled by [`AsyncContext::shutdown`] with an
//!   `-ECANCELED` completion, so no `user_data` is lost.
//! - The thread is exiting or the process group exited. Blocked operations
//!   unwind through the same `interruptible` waits the synchronous syscalls
//!   use, so the group-exit SIGKILL broadcast unblocks them.
//! - A pending `SIGKILL` interrupted the idle wait. Workers never re-enter
//!   userspace, where fatal-signal delivery normally happens, so the idle
//!   path probes the thread's pending-signal set itself (`kill -9` on a
//!   worker tid ends the worker). Handler-visible fatal signals other than
//!   `SIGKILL` still require a userspace return and are out of scope.
//! - The last ordinary (non-worker) thread of the process is gone, observed
//!   as `Process::has_only_in_process_workers` — an explicit per-process
//!   worker counter maintained by the spawn/retire paths below. Workers
//!   re-evaluate this at least every [`IDLE_PARK_BOUND`]; without it the
//!   final ordinary thread's plain exit would leave parked workers holding
//!   membership forever and `waitpid` would never complete.
//!
//! # Visibility and accounting
//!
//! Unlike io_uring's kernel-side io-wq threads, a kasyncio worker is a real
//! published thread of the process: its tid is globally visible (`tgkill`
//! can target it), it appears in process task listings, and its CPU time is
//! charged to the process. This is the intended phase-1 semantics (see
//! `docs/design.md`); revisiting it is a syscall-integration decision, not
//! an implementation accident.
//!
//! The spawn context must be a sleepable user-thread context (the submit
//! path): spawning walks the cgroup and identity allocators.

use alloc::sync::Arc;

use kerrno::{KError, KResult};
use kpoll::IoEvents;
use ksignal::Signo;
use ktask::future::{block_on, interruptible, poll_io, sleep, timeout};
use ktime_types::TimeSpan;

use crate::context::{AsyncContext, CompletedOp, PreparedOp, WorkSource};

/// Upper bound on workers per context.
///
/// Queued operations beyond the cap wait for a worker to drain; this bounds
/// kernel-stack usage per context while allowing a few concurrent blocking
/// operations.
pub(crate) const WORKER_CAP: u32 = 4;

/// Task-name prefix of every kasyncio worker thread, followed by
/// `{pid}/{ordinal}`. Purely for identification in listings and logs; the
/// retirement decision uses `Process::has_only_in_process_workers`, not the
/// name.
const WORKER_NAME_PREFIX: &str = "kaiow/";

/// Upper bound of one idle park.
///
/// The park is interruptible and normally ends through a wake or a signal;
/// the bound only guarantees that the retirement conditions (notably the
/// last-ordinary-thread scan) are re-evaluated periodically without needing
/// a cross-context notifier.
const IDLE_PARK_BOUND: TimeSpan = TimeSpan::from_millis(200);

/// Backoff base for consecutive idle-park registration failures, doubled per
/// consecutive failure up to [`PARK_FAILURE_BACKOFF_MAX_MS`]. Without it, a
/// persistently failing `PollSet` registration would spin without any
/// blocking point.
const PARK_FAILURE_BACKOFF_BASE_MS: u64 = 10;
/// Backoff ceiling for consecutive idle-park registration failures.
const PARK_FAILURE_BACKOFF_MAX_MS: u64 = 320;

/// Ensures a worker exists when queued work exceeds idle capacity.
///
/// Called from the submit path (sleepable user-thread context). The slot is
/// reserved under the context lock before spawning so concurrent submitters
/// cannot oversubscribe the cap; a failed spawn releases the reservation.
///
/// # Errors
///
/// Forwards thread-clone, publication, or commit errors.
pub(crate) fn ensure_worker(ctx: &Arc<AsyncContext>) -> KResult<()> {
    ensure_worker_with(ctx, spawn_worker)
}

/// [`ensure_worker`] with an injectable spawner, so tests can exercise the
/// failed-spawn reservation release.
fn ensure_worker_with(
    ctx: &Arc<AsyncContext>,
    spawn: impl FnOnce(&Arc<AsyncContext>, u32) -> KResult<ktask::KtaskRef>,
) -> KResult<()> {
    if !ctx.try_reserve_worker() {
        return Ok(());
    }
    if let Err(err) = spawn(ctx, ctx.next_worker_ordinal()) {
        ctx.unreserve_worker();
        return Err(err);
    }
    Ok(())
}

/// Spawns one worker thread of the current process, returning its task
/// handle (used by tests to observe the worker's scheduler state).
fn spawn_worker(ctx: &Arc<AsyncContext>, ordinal: u32) -> KResult<ktask::KtaskRef> {
    // Prepare the entry before the thread clone so a failed allocation leaves
    // no partially prepared thread state behind.
    let worker_ctx = ctx.clone();
    let entry = ktask::PreparedTaskEntry::new(move || worker_loop(worker_ctx))?;
    let current_thread = kprocess::current_user_thread();
    let prepared = current_thread.prepare_thread_clone()?;
    let name = alloc::format!(
        "{WORKER_NAME_PREFIX}{}/{}",
        prepared.process().pid(),
        ordinal
    );
    debug!("io_uring spawn worker {name}");
    let page_table_root = prepared.page_table_root();
    let process = prepared.process().clone();
    let (thread, task_number) = prepared.into_parts();

    let mut task = ktask::TaskInner::new_user(entry, name, task_number, thread);
    // Mirror the clone syscall: copy the creator's affinity and install the
    // process page-table root so kernel-mode user-VA access resolves against
    // the owning process's address space.
    task.set_cpumask(ktask::current().cpumask());
    task.ctx_mut().set_page_table_root(page_table_root);
    let published = kprocess::publish_user_task(task)?;
    // Attach between publication and activation: the worker cannot be
    // scheduled before `commit`, so it can never observe an un-attached
    // count and detach from zero. The transient over-count direction
    // (membership published, count not yet) is the conservative one. The
    // commit cannot fail here — its finalize closure is infallible — so no
    // rollback detach is needed.
    process.attach_in_process_worker();
    published.commit(|_| Ok(()))
}

/// Worker entry loop: dispatch queued operations, park when idle, retire on
/// shutdown, process exit, `SIGKILL`, or the last ordinary thread leaving.
fn worker_loop(ctx: Arc<AsyncContext>) {
    let mut park_failures: u32 = 0;
    loop {
        match ctx.pop_ready() {
            Some(op) => {
                ctx.account_inflight();
                let res = execute(&op);
                ctx.publish_completion(CompletedOp {
                    user_data: op.user_data,
                    res,
                });
            }
            None => {
                if should_retire(&ctx) {
                    break;
                }
                match park_until_work(&ctx) {
                    ParkOutcome::Work | ParkOutcome::ParkBound => park_failures = 0,
                    ParkOutcome::Interrupted => {
                        // A signal interrupted the idle wait. Signals without
                        // a group exit only re-run the retire checks; a
                        // pending SIGKILL must end the worker, which never
                        // returns to userspace for fatal-signal delivery.
                        park_failures = 0;
                        if pending_sigkill() {
                            break;
                        }
                    }
                    ParkOutcome::InterruptRegistrationFailed => {
                        // `interruptible`'s own wait registration failed
                        // (e.g. PollSet OOM): same hot-loop hazard as the
                        // poll_io path, so back off identically.
                        park_failures = park_failures.saturating_add(1);
                        let shift = park_failures.saturating_sub(1).min(6);
                        let backoff_ms = PARK_FAILURE_BACKOFF_BASE_MS
                            .saturating_mul(1 << shift)
                            .min(PARK_FAILURE_BACKOFF_MAX_MS);
                        debug!("io_uring idle registration failed, backing off {backoff_ms}ms");
                        block_on(sleep(TimeSpan::from_millis(backoff_ms)));
                    }
                    ParkOutcome::RegistrationFailed(err) => {
                        // `poll_io`'s inner `interruptible` compresses a
                        // signal-induced interruption into
                        // `KError::Interrupted` (one-shot consumption), so
                        // the SIGKILL check must run on that cause too —
                        // otherwise `kill -9` has a window where the signal
                        // is swallowed by the backoff branch.
                        if err == KError::Interrupted {
                            park_failures = 0;
                            if pending_sigkill() {
                                break;
                            }
                        } else {
                            // Persistently failing registrations (notably
                            // `PollRegisterError` OOM) must not spin: back
                            // off before the next park attempt.
                            park_failures = park_failures.saturating_add(1);
                            let shift = park_failures.saturating_sub(1).min(6);
                            let backoff_ms = PARK_FAILURE_BACKOFF_BASE_MS
                                .saturating_mul(1 << shift)
                                .min(PARK_FAILURE_BACKOFF_MAX_MS);
                            debug!("io_uring idle park failed, backing off {backoff_ms}ms: {err}");
                            block_on(sleep(TimeSpan::from_millis(backoff_ms)));
                        }
                    }
                }
            }
        }
        if should_retire(&ctx) {
            break;
        }
    }
    ctx.retire_worker();
    // Detach before membership removal, per the detach contract on
    // `Process::detach_in_process_worker`: the transient direction keeps
    // `has_only_in_process_workers` conservative for sibling workers.
    let process = kprocess::current_user_thread().process().clone();
    process.detach_in_process_worker();
    // Exit as a normal thread of the process without publishing a status: when
    // the worker is the last member its teardown still runs the process tail,
    // but the exit status belongs to the ordinary threads. `group_exit =
    // false`: the leader's exit already broadcast SIGKILL to surviving
    // siblings, which is what unwinds any in-flight operation.
    posix_process::do_exit_worker();
}

/// Spike stand-in for the operation table: every queued operation completes
/// successfully. The syscall steps replace this with the real dispatch.
fn execute(op: &PreparedOp) -> i32 {
    let _ = op;
    0
}

/// How one idle park ended.
enum ParkOutcome {
    /// Work arrived (or the context shut down).
    Work,
    /// The park bound expired without work or a signal.
    ParkBound,
    /// A task interrupt (`Interrupted::is_task_interrupt`) interrupted the
    /// wait.
    Interrupted,
    /// The wait registration inside `interruptible` itself failed (no task
    /// interrupt involved).
    InterruptRegistrationFailed,
    /// The wait machinery itself failed (e.g. `PollSet` registration OOM).
    RegistrationFailed(KError),
}

/// Parks the worker until work arrives, the context shuts down, a signal
/// interrupts, or [`IDLE_PARK_BOUND`] expires — whichever comes first.
fn park_until_work(ctx: &Arc<AsyncContext>) -> ParkOutcome {
    ctx.enter_idle();
    let outcome = block_on(timeout(
        Some(IDLE_PARK_BOUND),
        interruptible(poll_io(&WorkSource(ctx), IoEvents::IN, false, || {
            // Single critical section via the same readiness the poll side
            // uses (`WorkSource::poll` → `work_readiness`); the ready/shutdown
            // predicate must keep exactly one definition.
            if ctx.work_readiness().contains(IoEvents::IN) {
                Ok(())
            } else {
                Err(KError::WouldBlock)
            }
        })),
    ));
    ctx.leave_idle();
    match outcome {
        Err(_) => ParkOutcome::ParkBound,
        Ok(Err(interrupted)) if interrupted.is_task_interrupt() => ParkOutcome::Interrupted,
        Ok(Err(_interrupted)) => ParkOutcome::InterruptRegistrationFailed,
        Ok(Ok(Ok(()))) => ParkOutcome::Work,
        Ok(Ok(Err(err))) => ParkOutcome::RegistrationFailed(err),
    }
}

/// Returns whether the worker must leave its loop: context shutdown, thread
/// exit in progress, process group exit, or the last ordinary thread gone.
fn should_retire(ctx: &Arc<AsyncContext>) -> bool {
    if ctx.is_shutdown() {
        return true;
    }
    let thread = kprocess::current_user_thread();
    if thread.is_exiting() || thread.process().is_group_exited() {
        return true;
    }
    thread.process().has_only_in_process_workers()
}

/// Returns whether the current worker thread has a pending `SIGKILL`.
fn pending_sigkill() -> bool {
    kprocess::current_user_thread()
        .signal_manager()
        .pending()
        .has(Signo::SIGKILL)
}

unittest::path_mod_test!(worker_tests, "../ut-tests/worker.rs");
