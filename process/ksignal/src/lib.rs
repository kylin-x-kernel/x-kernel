// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel signal handling and delivery.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

mod tests;

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

#[crate_interface::def_interface]
pub trait CurrentSignalDispatch {
    fn send_sig_current(signo: Signo) -> kerrno::KResult<()>;
}

/// Sends a signal to the current user thread.
pub fn send_sig_current(signo: Signo) -> kerrno::KResult<()> {
    crate_interface::call_interface!(CurrentSignalDispatch::send_sig_current, signo)
}
