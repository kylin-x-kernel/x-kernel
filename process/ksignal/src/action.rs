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
use posix_types::k_sigaction;

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
    #[derive(Default, Debug, Clone, Copy)]
    pub struct SignalActionFlags: c_ulong {
        const NOCLDSTOP = SA_NOCLDSTOP as _;
        const NOCLDWAIT = SA_NOCLDWAIT as _;
        const SIGINFO = SA_SIGINFO as _;
        const NODEFER = SA_NODEFER as _;
        const RESETHAND = SA_RESETHAND as _;
        const RESTART = SA_RESTART as _;
        const ONSTACK = SA_ONSTACK as _;
        const RESTORER = 0x4000000;
    }
}

#[derive(Debug, Default, Clone)]
pub enum SignalDisposition {
    #[default]
    /// Use the default signal action.
    Default,
    /// Ignore the signal.
    Ignore,
    /// Custom signal handler.
    Handler(unsafe extern "C" fn(i32)),
}

/// Signal action. Corresponds to `struct sigaction` in libc.
#[derive(Debug, Clone, Default)]
pub struct SignalAction {
    pub flags: SignalActionFlags,
    pub mask: SignalSet,
    pub disposition: SignalDisposition,
    pub restorer: __sigrestore_t,
}

impl From<SignalAction> for kernel_sigaction {
    fn from(value: SignalAction) -> Self {
        let value = k_sigaction::from(value);

        Self {
            sa_handler_kernel: value.handler,
            sa_flags: value.flags,
            #[cfg(sa_restorer)]
            sa_restorer: value.restorer,
            sa_mask: value.mask.into(),
        }
    }
}

impl From<SignalAction> for k_sigaction {
    fn from(value: SignalAction) -> Self {
        Self {
            handler: match value.disposition {
                SignalDisposition::Default => None,
                SignalDisposition::Ignore => sig_ign(),
                SignalDisposition::Handler(handler) => Some(handler),
            },
            flags: value.flags.bits() as _,
            restorer: value.restorer,
            mask: value.mask.into(),
        }
    }
}

impl From<kernel_sigaction> for SignalAction {
    fn from(value: kernel_sigaction) -> Self {
        k_sigaction {
            handler: value.sa_handler_kernel,
            flags: value.sa_flags,
            #[cfg(sa_restorer)]
            restorer: value.sa_restorer,
            #[cfg(not(sa_restorer))]
            restorer: None,
            mask: value.sa_mask.into(),
        }
        .into()
    }
}

impl From<k_sigaction> for SignalAction {
    fn from(value: k_sigaction) -> Self {
        let flags = SignalActionFlags::from_bits_truncate(value.flags);
        let disposition = {
            match value.handler {
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
            value.restorer
        } else {
            None
        };
        #[cfg(not(sa_restorer))]
        let restorer = None;

        SignalAction {
            flags,
            mask: value.mask.into(),
            disposition,
            restorer,
        }
    }
}
