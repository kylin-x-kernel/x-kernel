// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Async helpers built on top of pollable I/O readiness.

use core::{future::poll_fn, task::Poll};

use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollRegisterError, PollRegistrations, Pollable};

/// A helper to wrap a synchronous non-blocking I/O function into an
/// asynchronous function.
///
/// # Arguments
///
/// * `pollable`: The pollable object to register for I/O events.
/// * `events`: The I/O events to wait for.
/// * `non_blocking`: If true, the function will return `KError::WouldBlock`
///   immediately when the I/O operation would block.
/// * `f`: The synchronous non-blocking I/O function to be wrapped. It should
///   return `KError::WouldBlock` when the operation would block.
pub async fn poll_io<P: Pollable, F: FnMut() -> KResult<T>, T>(
    pollable: &P,
    events: IoEvents,
    non_blocking: bool,
    f: F,
) -> KResult<T> {
    poll_io_with_cause(pollable, events, non_blocking, f).await?
}

/// Waits for I/O while retaining the source of interruption.
///
/// The outer error belongs to task interruption or its registration; operation
/// and I/O registration errors remain in the inner result. Like [`poll_io`],
/// task interruption is checked before each operation poll. Poll this future
/// in task context without locks that prohibit blocking; dropping it removes
/// its wait registrations. No Linux syscall restart policy is applied here.
pub async fn poll_io_with_cause<P: Pollable, F: FnMut() -> KResult<T>, T>(
    pollable: &P,
    events: IoEvents,
    non_blocking: bool,
    mut operation_fn: F,
) -> Result<KResult<T>, super::Interrupted> {
    let mut registrations = PollRegistrations::new();
    super::interruptible(poll_fn(move |cx| match operation_fn() {
        Ok(value) => Poll::Ready(Ok(value)),
        Err(KError::WouldBlock) => {
            if non_blocking {
                return Poll::Ready(Err(KError::WouldBlock));
            }
            let mut context = registrations.context(cx);
            if let Err(error) = pollable.register(&mut context, events) {
                return Poll::Ready(Err(map_register_error(error)));
            }
            drop(context);
            match operation_fn() {
                Ok(value) => Poll::Ready(Ok(value)),
                Err(KError::WouldBlock) => Poll::Pending,
                Err(e) => Poll::Ready(Err(e)),
            }
        }
        Err(e) => Poll::Ready(Err(e)),
    }))
    .await
}

fn map_register_error(error: PollRegisterError) -> KError {
    match error {
        PollRegisterError::NoMemory | PollRegisterError::IdExhausted => KError::NoMemory,
        PollRegisterError::InvalidState => KError::InvalidInput,
    }
}
