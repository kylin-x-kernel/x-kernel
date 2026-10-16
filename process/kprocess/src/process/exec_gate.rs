// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-wide `exec` serialization.
//!
//! Two sibling threads that call `execve` at the same time each run a
//! `de_thread` sibling sweep that sends `SIGKILL` to the other and then blocks
//! in `join()`, so neither can finish: a mutual deadlock. Linux avoids it with
//! the process-wide `cred_guard_mutex`. This gate provides the same
//! serialization. The losing thread waits interruptibly and is killed by the
//! winner's sweep instead of joining it.

use alloc::sync::Arc;
use core::{
    future::poll_fn,
    sync::atomic::{AtomicBool, Ordering},
    task::Poll,
};

use kpoll::{PollRegistrations, PollSet};
use ktask::{KtaskRef, future::block_on};

use crate::Thread;

/// Serializes concurrent `exec` within one process.
pub(crate) struct ExecGate {
    in_progress: AtomicBool,
    released: Arc<PollSet>,
}

impl ExecGate {
    pub(crate) fn new() -> Self {
        Self {
            in_progress: AtomicBool::new(false),
            released: Arc::new(PollSet::new()),
        }
    }

    fn try_acquire(&self) -> bool {
        self.in_progress
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
    }

    fn release(&self) {
        self.in_progress.store(false, Ordering::Release);
        self.released.wake();
    }

    /// Acquires the gate, waiting interruptibly.
    ///
    /// Returns `false` only when the *winning exec's* sweep is killing `thread`
    /// (it is a marked `exec` sweep target). Any other wake — a non-fatal
    /// signal, an unrelated interrupt, or registration pressure — is not
    /// authoritative and the wait continues, so it cannot be mistaken for the
    /// winner's kill.
    fn acquire(&self, current: &KtaskRef, thread: &Thread) -> bool {
        loop {
            if self.try_acquire() {
                return true;
            }
            let mut release_registrations = PollRegistrations::new();
            let mut interrupt_registrations = PollRegistrations::new();
            let woken = block_on(poll_fn(|cx| {
                if self.try_acquire() {
                    return Poll::Ready(false);
                }
                let mut context = release_registrations.context(cx);
                if context.register(&self.released).is_err() {
                    return Poll::Ready(true);
                }
                drop(context);
                if self.try_acquire() {
                    return Poll::Ready(false);
                }
                let mut interrupt_context = interrupt_registrations.context(cx);
                match current.poll_interrupt(&mut interrupt_context) {
                    Ok(Poll::Ready(())) => Poll::Ready(true),
                    Ok(Poll::Pending) => Poll::Pending,
                    Err(_) => Poll::Ready(true),
                }
            }));
            if !woken {
                return true;
            }
            if thread.is_exec_sweep_target() {
                return false;
            }
            // Spurious or non-fatal wake, or registration pressure: back off and
            // re-register to close the check/wait race.
            ktask::yield_now();
        }
    }
}

/// Releases the process `exec` gate on drop.
pub struct ExecGateGuard<'a> {
    gate: &'a ExecGate,
}

impl Drop for ExecGateGuard<'_> {
    fn drop(&mut self) {
        self.gate.release();
    }
}

impl super::Process {
    /// Acquires the process-wide `exec` gate, waiting interruptibly.
    ///
    /// Returns `None` only when the winning exec's sweep killed `thread`; the
    /// caller must terminate instead of proceeding with `exec`.
    pub fn acquire_exec_gate(
        &self,
        current: &KtaskRef,
        thread: &Thread,
    ) -> Option<ExecGateGuard<'_>> {
        if self.exec_gate.acquire(current, thread) {
            Some(ExecGateGuard {
                gate: &self.exec_gate,
            })
        } else {
            None
        }
    }
}
