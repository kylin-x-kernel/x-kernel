// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Context-policy tests not covered by the worker lifecycle tests in
//! `worker.rs` (which observe the spawned threads directly).

use kerrno::KError;
use ktask::future::{block_on, poll_io};
use unittest::def_test;

use crate::{
    context::{AsyncContext, CompletionSource, PreparedOp, READY_LIMIT},
    worker::ensure_worker,
};

#[def_test(user, serial)]
fn shutdown_context_refuses_worker_spawn() {
    let ctx = AsyncContext::new().expect("context creation");

    ctx.push_ready(PreparedOp { user_data: 7 })
        .expect("op below queue limit");
    ctx.shutdown();
    ensure_worker(&ctx).expect("ensure_worker must not fail on a shut-down context");

    // A shut-down context must not spawn workers: there is nothing left to
    // serve, and a fresh thread would only have to observe shutdown and exit.
    assert_eq!(ctx.total_workers(), 0);
}

#[def_test(user, serial)]
fn full_ready_queue_rejects_until_drained() {
    let ctx = AsyncContext::new().expect("context creation");

    for user_data in 0..READY_LIMIT as u64 {
        ctx.push_ready(PreparedOp { user_data })
            .expect("fills up to the limit must succeed");
    }
    assert!(
        ctx.push_ready(PreparedOp {
            user_data: READY_LIMIT as u64
        })
        .is_err(),
        "the queue past its limit must report WouldBlock"
    );

    let mut drained: alloc::vec::Vec<u64> = alloc::vec::Vec::with_capacity(READY_LIMIT);
    while let Some(op) = ctx.pop_ready() {
        drained.push(op.user_data);
    }
    assert_eq!(drained.len(), READY_LIMIT, "drain must observe every op");
    assert_eq!(drained[0], 0, "FIFO: the first queued op drains first");
    assert_eq!(
        drained[READY_LIMIT - 1],
        READY_LIMIT as u64 - 1,
        "FIFO: the last queued op drains last"
    );
    ctx.push_ready(PreparedOp { user_data: 0 })
        .expect("drained queue must accept again");
}

#[def_test(user, serial)]
fn try_reserve_worker_enforces_the_pool_cap() {
    let ctx = AsyncContext::new().expect("context creation");

    for user_data in 0..5 {
        ctx.push_ready(PreparedOp { user_data })
            .expect("seed op below queue limit");
    }

    // WORKER_CAP consecutive reservations must succeed, then the cap must
    // hold even though queued work still exceeds idle capacity.
    for _ in 0..4 {
        assert!(ctx.try_reserve_worker(), "cap not reached yet");
    }
    assert!(
        !ctx.try_reserve_worker(),
        "reservation past WORKER_CAP must fail in the same critical section"
    );
    assert_eq!(ctx.total_workers(), 4);

    for _ in 0..4 {
        ctx.unreserve_worker();
    }
    assert_eq!(ctx.total_workers(), 0);
}

#[def_test(user, serial)]
fn shutdown_cancels_queued_ops_with_canceled_result() {
    let ctx = AsyncContext::new().expect("context creation");

    for user_data in 1..=3 {
        ctx.push_ready(PreparedOp { user_data })
            .expect("op below queue limit");
    }

    // No worker was spawned, so every queued op is still undispatched and
    // must come back as a cancellation, keeping its user_data observable.
    ctx.shutdown();

    assert!(!ctx.has_ready(), "shutdown must drain the ready queue");

    // The completion wait path (`CompletionSource` through `poll_io`, the
    // path the enter syscall will block on) must observe the published
    // cancellations immediately.
    block_on(poll_io(
        &CompletionSource(&ctx),
        kpoll::IoEvents::IN,
        false,
        || {
            if ctx.has_completions() {
                Ok(())
            } else {
                Err(KError::WouldBlock)
            }
        },
    ))
    .expect("completion source must report ready after shutdown");

    let completions: alloc::vec::Vec<_> = ctx.take_completions().into_iter().collect();
    assert_eq!(completions.len(), 3);
    for (index, completion) in completions.iter().enumerate() {
        assert_eq!(completion.user_data, index as u64 + 1);
        assert_eq!(
            completion.res,
            -kerrno::LinuxError::ECANCELED.into_raw()
        );
    }

    // A submission racing the shutdown drain must be rejected with the
    // documented error, never silently queued on a context no worker will
    // serve again, and must leave both queues untouched.
    assert!(matches!(
        ctx.push_ready(PreparedOp { user_data: 9 }),
        Err(KError::BadFileDescriptor)
    ));
    assert!(!ctx.has_ready());
    assert!(ctx.take_completions().is_empty());
}
