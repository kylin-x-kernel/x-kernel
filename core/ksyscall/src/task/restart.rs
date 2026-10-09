// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `restart_syscall(2)` adapter.
//!
//! The syscall carries no ABI arguments. Continuation state lives on the
//! current thread's restart-block; this adapter only consumes it.

use kerrno::{KError, KResult, LinuxError};
use kprocess::current_user_thread;

use crate::sync::{FutexWaitRestartBlock, resume_futex_wait};

/// Restarts a syscall that returned `ERESTART_RESTARTBLOCK`.
///
/// The signal-return path rewrites the syscall number to this entry when no
/// handler is delivered. Userspace does not call it. An empty slot, or a
/// payload this adapter does not recognize, is Linux `do_no_restart_syscall`
/// and returns `EINTR`. A handler that already published `EINTR` has cleared
/// the slot.
///
/// Must run on the current user thread. The restarted wait may itself return
/// `ERESTART_RESTARTBLOCK` if it is interrupted again.
///
/// # Errors
///
/// * `EINTR` — no restart-block is armed.
/// * otherwise the error of the resumed syscall.
pub(crate) fn sys_restart_syscall() -> KResult<isize> {
    let current = current_user_thread();
    if let Some(wait) = current.take_restart_block::<FutexWaitRestartBlock>() {
        return resume_futex_wait(wait);
    }
    current.clear_restart_block();
    Err(KError::from(LinuxError::EINTR))
}
