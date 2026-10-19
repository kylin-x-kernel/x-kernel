// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Readiness-first waits for explicitly opted-in file operations.

use kerrno::{KError, KResult};
use kpoll::{IoEvents, Pollable};
use ktask::future::poll_io_with_cause;

/// Tries file I/O, waiting only when the operation would block.
///
/// Nonblocking calls never register a wait. A signal interrupting a blocking
/// wait triggers one final operation attempt: readiness, EOF and other errors
/// take precedence over [`KError::IoWaitInterrupted`]. Ordinary operation and
/// registration errors are never promoted to that marker.
///
/// Poll in sleepable task context. `operation_fn` may acquire sleepable object
/// locks, but must return `WouldBlock` instead of waiting for I/O readiness and
/// release its locks before returning. It owns any partial progress: an enclosing
/// transfer must report completed bytes before propagating an interruption. The marker
/// defaults to EINTR; only audited syscall exits may request syscall restart.
pub async fn poll_file_io<P: Pollable, F: FnMut() -> KResult<T>, T>(
    pollable: &P,
    events: IoEvents,
    is_nonblocking: bool,
    mut operation_fn: F,
) -> KResult<T> {
    match operation_fn() {
        Err(KError::WouldBlock) if !is_nonblocking => {}
        result => return result,
    }

    match poll_io_with_cause(pollable, events, false, &mut operation_fn).await {
        Ok(result) => result,
        Err(interrupted) if interrupted.is_signal() => match operation_fn() {
            Err(KError::WouldBlock) => Err(KError::IoWaitInterrupted),
            result => result,
        },
        Err(error) => Err(error.into()),
    }
}

unittest::path_mod_test!(tests, "../ut-tests/poll.rs");
