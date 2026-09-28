// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Signal actions and sigaction conversions.

use core::ffi::c_ulong;

use bitflags::bitflags;
use linux_raw_sys::{
    general::{
        __sigrestore_t, SA_NOCLDSTOP, SA_NOCLDWAIT, SA_NODEFER, SA_ONSTACK, SA_RESETHAND,
        SA_RESTART, SA_SIGINFO, kernel_sigaction,
    },
    signal_macros::sig_ign,
};

use crate::SignalSet;

/// Default actions for signals when no custom handler is set.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefaultSignalAction {
    /// Terminate the process.
    Terminate,
    /// Ignore the signal.
    Ignore,
    /// Terminate the process and generate a core dump.
    CoreDump,
    /// Stop (suspend) the process.
    Stop,
    /// Continue the process if currently stopped.
    Continue,
}

/// Operating system actions to take when a signal is delivered.
///
/// These represent the actions the kernel should take after signal
/// processing, distinct from user-defined signal handlers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignalOSAction {
    /// Terminate the process immediately.
    Terminate,
    /// Generate a core dump and terminate the process.
    CoreDump,
    /// Suspend the process execution.
    Stop,
    /// Resume the process if it was stopped.
    Continue,
    /// A signal handler was invoked; no additional OS action needed.
    Handler,
}

bitflags! {
    /// `sa_flags` bits carried by a [`SignalAction`].
    ///
    /// Values mirror the Linux `SA_*` ABI. Unknown bits received from user
    /// space are dropped by `from_bits_truncate` during ABI conversion.
    #[derive(Default, Debug, Clone, Copy)]
    pub struct SignalActionFlags: c_ulong {
        /// Do not raise `SIGCHLD` when children stop (`SA_NOCLDSTOP`).
        const NOCLDSTOP = SA_NOCLDSTOP as _;
        /// Do not create zombies, request child autoreap (`SA_NOCLDWAIT`).
        const NOCLDWAIT = SA_NOCLDWAIT as _;
        /// Handler takes the `siginfo` ABI (`SA_SIGINFO`).
        const SIGINFO = SA_SIGINFO as _;
        /// Do not add the signal itself to the handler mask (`SA_NODEFER`).
        const NODEFER = SA_NODEFER as _;
        /// Restore the default disposition on handler entry (`SA_RESETHAND`).
        const RESETHAND = SA_RESETHAND as _;
        /// Restart interrupted syscalls instead of failing with `EINTR` (`SA_RESTART`).
        const RESTART = SA_RESTART as _;
        /// Invoke the handler on the alternate signal stack (`SA_ONSTACK`).
        const ONSTACK = SA_ONSTACK as _;
        /// Use the user-provided trampoline on ABIs with a restorer slot (`SA_RESTORER`).
        const RESTORER = 0x4000000;
    }
}

/// Disposition configured for one signal.
#[derive(Debug, Default, Clone)]
pub enum SignalDisposition {
    /// Use the default signal action.
    #[default]
    Default,
    /// Ignore the signal (`SIG_IGN`).
    Ignore,
    /// Invoke a user handler (`SIG_DFL` replaced by an address).
    ///
    /// The handler is an opaque user entry point typed as a C function
    /// pointer; the kernel calls it only through the constructed user frame,
    /// never directly from kernel code.
    Handler(unsafe extern "C" fn(i32)),
}

/// Signal action. Corresponds to `struct sigaction` in libc.
#[derive(Debug, Clone, Default)]
pub struct SignalAction {
    /// `SA_*` behavior flags.
    pub flags: SignalActionFlags,
    /// Signals blocked while the handler runs.
    pub mask: SignalSet,
    /// What to do when the signal is delivered.
    pub disposition: SignalDisposition,
    /// User-space `sigreturn` trampoline entry, used only when the
    /// architecture ABI has a restorer slot and the user sets `SA_RESTORER`.
    pub restorer: __sigrestore_t,
}

impl From<SignalAction> for kernel_sigaction {
    fn from(value: SignalAction) -> Self {
        Self {
            sa_handler_kernel: match value.disposition {
                SignalDisposition::Default => None,
                SignalDisposition::Ignore => sig_ign(),
                SignalDisposition::Handler(handler) => Some(handler),
            },
            sa_flags: value.flags.bits() as _,
            #[cfg(sa_restorer)]
            sa_restorer: value.restorer,
            sa_mask: value.mask.into(),
        }
    }
}

impl From<kernel_sigaction> for SignalAction {
    fn from(value: kernel_sigaction) -> Self {
        let flags = SignalActionFlags::from_bits_truncate(value.sa_flags);
        let disposition = {
            match value.sa_handler_kernel {
                None => {
                    // SIG_DFL
                    SignalDisposition::Default
                }
                Some(h) if h as usize == 1 => {
                    // SIG_IGN
                    SignalDisposition::Ignore
                }
                Some(h) => {
                    // Custom signal handler
                    SignalDisposition::Handler(h)
                }
            }
        };

        #[cfg(sa_restorer)]
        let restorer = if flags.contains(SignalActionFlags::RESTORER) {
            value.sa_restorer
        } else {
            None
        };
        #[cfg(not(sa_restorer))]
        let restorer = None;

        SignalAction {
            flags,
            mask: value.sa_mask.into(),
            disposition,
            restorer,
        }
    }
}
