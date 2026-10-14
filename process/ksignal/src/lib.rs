// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel signal handling and delivery.
//!
//! This crate owns the data model and per-process/per-thread state of Linux
//! signals: signal numbers and sets ([`Signo`], [`SignalSet`]), the
//! `siginfo_t` payload ([`SignalInfo`]), dispositions ([`SignalAction`]),
//! pending queues ([`PendingSignals`]), and the process/thread managers that
//! decide queuing, blocking, and handler-frame construction
//! ([`api::ProcessSignalManager`], [`api::ThreadSignalManager`]).
//! Architecture-specific
//! frame layouts and the signal trampoline live in [`arch`] and
//! [`map_signal_trampoline`].
//!
//! Preferred entry points:
//!
//! - `ThreadSignalManager::check_signals` — user-return signal dispatch;
//! - `ProcessSignalManager::send_signal` — process-directed delivery;
//! - `register_signal_observer` — kernel-side dequeue policy hooks.
//!
//! Syscall ABI decoding, permission checks, and the user-loop that turns
//! [`SignalOsAction`] into process teardown belong to `ksyscall` and the
//! posix process runtime.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

unittest::path_mod_test!(signal_manager_tests, "../ut-tests/signal_manager.rs");

pub mod api;
pub use api::{SignalDequeueAction, register_signal_observer, unregister_signal_observer};
pub mod arch;

mod action;
pub use action::*;

mod pending;
pub use pending::*;

mod types;
pub use types::*;

mod trampoline;
pub use trampoline::map_signal_trampoline;

#[kiface::interface]
/// Provider contract for delivering a kernel-originated signal to the
/// current user thread.
///
/// `kprocess` implements this via `kiface::provide`; the trait lets `ksignal`
/// dependents (for example `ktimer`) request current-thread signal delivery
/// without a direct `kprocess` dependency.
pub trait CurrentSignalDispatch {
    /// Queues `signo` on the current user thread.
    ///
    /// # Errors
    ///
    /// Returns an error when the current task has no user-thread runtime
    /// (for example a kernel worker) and cannot receive user signals.
    fn send_sig_current(signo: Signo) -> kerrno::KResult<()>;
}

/// Sends a signal to the current user thread through the registered
/// [`CurrentSignalDispatch`] provider.
///
/// # Errors
///
/// Returns the provider's error; see [`CurrentSignalDispatch::send_sig_current`].
pub fn send_sig_current(signo: Signo) -> kerrno::KResult<()> {
    CurrentSignalDispatch::send_sig_current(signo)
}
