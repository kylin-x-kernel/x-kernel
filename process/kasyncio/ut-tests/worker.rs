// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;

use kerrno::KError;
use ktask::{TaskState, WaitQueue};
use ktime_types::TimeSpan;
use unittest::def_test;

use super::*;

/// Waits for a scheduler-observable condition with short parks instead
/// of a busy spin, failing the test after a bounded deadline.
fn wait_until(condition: impl Fn() -> bool) {
    let deadline = khal::time::monotonic_time() + TimeSpan::from_secs(5);
    let queue = WaitQueue::new();
    while !condition() {
        assert!(
            khal::time::monotonic_time() < deadline,
            "kasyncio worker test timed out"
        );
        queue.wait_timeout(TimeSpan::from_millis(1));
    }
}

/// Spawns one worker directly (bypassing the growth policy) so the test
/// holds the worker's task handle for state observation.
fn spawn_direct(ctx: &Arc<AsyncContext>) -> ktask::KtaskRef {
    ctx.push_ready(PreparedOp { user_data: 1 })
        .expect("seed op below queue limit");
    assert!(ctx.try_reserve_worker(), "fresh context must reserve");
    spawn_worker(ctx, ctx.next_worker_ordinal()).expect("worker spawn must succeed")
}

#[def_test(user, serial)]
fn worker_executes_queued_op_and_exits_on_shutdown() {
    let ctx = AsyncContext::new().expect("context creation");
    ctx.push_ready(PreparedOp { user_data: 0xABCD })
        .expect("op below queue limit");
    ensure_worker(&ctx).expect("worker spawn must succeed");

    wait_until(|| ctx.has_completions());
    let completions: alloc::vec::Vec<_> = ctx.take_completions().into_iter().collect();
    assert_eq!(completions.len(), 1);
    assert_eq!(completions[0].user_data, 0xABCD);
    assert_eq!(completions[0].res, 0);

    ctx.shutdown();
    wait_until(|| ctx.total_workers() == 0);
}

#[def_test(user, serial)]
fn shutdown_wakes_an_idle_parked_worker() {
    let ctx = AsyncContext::new().expect("context creation");
    let worker = spawn_direct(&ctx);
    wait_until(|| ctx.has_completions());
    wait_until(|| worker.state() == TaskState::Blocked);

    ctx.shutdown();
    wait_until(|| ctx.total_workers() == 0);
}

#[def_test(user, serial)]
fn interrupt_alone_does_not_retire_an_idle_worker() {
    // A signal without a group exit and without a pending SIGKILL only
    // interrupts the idle wait; the worker observes no retire condition
    // and goes back to sleep.
    let ctx = AsyncContext::new().expect("context creation");
    let worker = spawn_direct(&ctx);
    wait_until(|| ctx.has_completions());
    wait_until(|| worker.state() == TaskState::Blocked);

    worker.interrupt();
    wait_until(|| worker.state() == TaskState::Blocked);
    assert_eq!(ctx.total_workers(), 1);

    ctx.shutdown();
    wait_until(|| ctx.total_workers() == 0);
}

#[def_test(user, serial)]
fn worker_executes_a_second_op_after_reparking() {
    // After the first op completes the worker parks again; a second
    // submission must wake it, complete, and leave the idle accounting
    // balanced. The idle observation tolerates the periodic park-bound
    // wake, so wait instead of asserting a point-in-time value.
    let ctx = AsyncContext::new().expect("context creation");
    let worker = spawn_direct(&ctx);
    wait_until(|| ctx.has_completions());
    wait_until(|| worker.state() == TaskState::Blocked);
    assert_eq!(ctx.total_workers(), 1);

    ctx.push_ready(PreparedOp { user_data: 2 })
        .expect("op below queue limit");
    wait_until(|| {
        ctx.take_completions()
            .into_iter()
            .any(|completion| completion.user_data == 2)
    });
    wait_until(|| worker.state() == TaskState::Blocked);
    wait_until(|| ctx.idle_workers() == 1);
}

#[def_test(user, serial)]
fn failed_spawn_releases_the_worker_reservation() {
    let ctx = AsyncContext::new().expect("context creation");
    ctx.push_ready(PreparedOp { user_data: 1 })
        .expect("seed op below queue limit");

    let result = ensure_worker_with(&ctx, |_ctx, _ordinal| Err(KError::WouldBlock));
    assert!(result.is_err(), "the injected spawner must fail");
    assert_eq!(ctx.total_workers(), 0, "failed spawn must release");

    // The released slot is immediately reusable for a real spawn.
    ensure_worker(&ctx).expect("worker spawn must succeed");
    wait_until(|| ctx.total_workers() == 1);
    ctx.shutdown();
    wait_until(|| ctx.total_workers() == 0);
}

#[def_test(user, serial)]
fn ordinary_thread_presence_blocks_worker_retirement() {
    // The test body runs on an ordinary (non-worker) thread of its
    // process, and no worker is attached, so the retirement condition
    // must stay false. The complementary case (only workers left) is
    // covered by `last_ordinary_thread_exit_retires_the_worker`.
    let thread = kprocess::current_user_thread();
    assert!(!thread.process().has_only_in_process_workers());
}

#[def_test(user, serial)]
fn last_ordinary_thread_exit_retires_the_worker() {
    use kprocess::{
        ForkAddressSpace, ForkFdTable, ForkFs, ForkParent, ForkSignalActions, ProcessForkConfig,
    };

    // Fork a child whose only ordinary thread spawns one worker and then
    // exits plainly — the exact shape of the stranded-worker hazard: no
    // group exit runs, so only the last-ordinary-thread condition can
    // retire the worker and let the child process tail complete.
    let parent_thread = kprocess::current_user_thread();
    let prepared = parent_thread
        .prepare_process_fork(ProcessForkConfig {
            parent: ForkParent::Caller,
            address_space: ForkAddressSpace::Private,
            fs: ForkFs::Shared,
            signal_actions: ForkSignalActions::Shared,
            fd_table: ForkFdTable::Private,
            namespace_flags: kns::NamespaceFlags::empty(),
            exit_signal: Some(Signo::SIGCHLD),
            cgroup_target: None,
        })
        .expect("fork must succeed");
    let child_process = prepared.process().clone();
    let page_table_root = prepared.page_table_root();
    let (child_thread, task_number) = prepared.into_parts();

    // The context is created on the parent; the child pushes its op and
    // spawns the worker, so the worker thread belongs to the child
    // process.
    let ctx = AsyncContext::new().expect("context creation");
    let child_ctx = ctx.clone();
    let child_entry = ktask::PreparedTaskEntry::new(move || {
        child_ctx
            .push_ready(PreparedOp { user_data: 1 })
            .expect("op below queue limit");
        ensure_worker(&child_ctx).expect("worker spawn must succeed");
        wait_for_op_completion(&child_ctx, 1);
        // Plain thread exit with a non-zero status: no group_exit
        // broadcast, is_last_thread is false while the worker is alive.
        // The worker's later retirement must not replace this status.
        posix_process::do_exit(42, false);
    })
    .expect("child entry preparation");
    let mut child = ktask::TaskInner::new_user(
        child_entry,
        alloc::string::String::from("kasyncio-test-child"),
        task_number,
        child_thread,
    );
    child.set_cpumask(ktask::current().cpumask());
    child.ctx_mut().set_page_table_root(page_table_root);
    kprocess::publish_user_task(child)
        .expect("child publication must succeed")
        .commit(|_| Ok(()))
        .expect("child activation must succeed");

    // The parked worker has no wake source here: only the bounded-park
    // re-check of the retirement condition can retire it, after which
    // its do_exit is the last membership removal and runs the child
    // process tail.
    wait_until(|| child_process.is_exited());
    assert!(
        child_process.is_waitable_zombie(),
        "the child must end as a waitable zombie for its parent"
    );
    assert_eq!(
        child_process.exit_code(),
        42,
        "the retiring worker must preserve the ordinary thread's exit status"
    );
    assert_eq!(ctx.total_workers(), 0);

    // Reap the child through the ordinary wait path — the real
    // waitpid-equivalent observable — so the test does not leak a
    // zombie into later serial cases.
    kprocess::wait_reap::assert_reap_zombie_process(&child_process);
}

/// Waits until the operation with `user_data` appears in the completions of
/// `ctx` (test-side observation for the fork child's context).
fn wait_for_op_completion(ctx: &Arc<AsyncContext>, user_data: u64) {
    wait_until(|| {
        ctx.take_completions()
            .into_iter()
            .any(|completion| completion.user_data == user_data)
    });
}
