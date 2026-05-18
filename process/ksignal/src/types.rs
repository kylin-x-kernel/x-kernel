// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Signal types, sets, and siginfo helpers.
use core::{fmt, mem};

use derive_more::{BitAnd, BitAndAssign, BitOr, BitOrAssign, Not};
use linux_raw_sys::general::{SI_KERNEL, SI_TIMER, SS_DISABLE, kernel_sigset_t, siginfo_t};
use posix_types::{k_sigaltstack, k_siginfo, k_sigset, k_sigval};
use strum::{EnumIter, FromRepr, IntoEnumIterator};

use crate::DefaultSignalAction;

/// Maximum number of signals supported.
pub const MAX_SIGNALS: usize = 64;

/// Signal number.
#[repr(u8)]
#[derive(Debug, Copy, Clone, PartialEq, Eq, PartialOrd, Ord, FromRepr, EnumIter)]
pub enum Signo {
    SIGHUP    = 1,
    SIGINT    = 2,
    SIGQUIT   = 3,
    SIGILL    = 4,
    SIGTRAP   = 5,
    SIGABRT   = 6,
    SIGBUS    = 7,
    SIGFPE    = 8,
    SIGKILL   = 9,
    SIGUSR1   = 10,
    SIGSEGV   = 11,
    SIGUSR2   = 12,
    SIGPIPE   = 13,
    SIGALRM   = 14,
    SIGTERM   = 15,
    SIGSTKFLT = 16,
    SIGCHLD   = 17,
    SIGCONT   = 18,
    SIGSTOP   = 19,
    SIGTSTP   = 20,
    SIGTTIN   = 21,
    SIGTTOU   = 22,
    SIGURG    = 23,
    SIGXCPU   = 24,
    SIGXFSZ   = 25,
    SIGVTALRM = 26,
    SIGPROF   = 27,
    SIGWINCH  = 28,
    SIGIO     = 29,
    SIGPWR    = 30,
    SIGSYS    = 31,
    SIGRTMIN  = 32,
    SIGRT1    = 33,
    SIGRT2    = 34,
    SIGRT3    = 35,
    SIGRT4    = 36,
    SIGRT5    = 37,
    SIGRT6    = 38,
    SIGRT7    = 39,
    SIGRT8    = 40,
    SIGRT9    = 41,
    SIGRT10   = 42,
    SIGRT11   = 43,
    SIGRT12   = 44,
    SIGRT13   = 45,
    SIGRT14   = 46,
    SIGRT15   = 47,
    SIGRT16   = 48,
    SIGRT17   = 49,
    SIGRT18   = 50,
    SIGRT19   = 51,
    SIGRT20   = 52,
    SIGRT21   = 53,
    SIGRT22   = 54,
    SIGRT23   = 55,
    SIGRT24   = 56,
    SIGRT25   = 57,
    SIGRT26   = 58,
    SIGRT27   = 59,
    SIGRT28   = 60,
    SIGRT29   = 61,
    SIGRT30   = 62,
    SIGRT31   = 63,
    SIGRT32   = 64,
}

impl Signo {
    /// Returns `true` if this is a real-time signal.
    pub fn is_realtime(&self) -> bool {
        *self >= Signo::SIGRTMIN
    }

    /// Returns the default action for this signal.
    pub fn default_action(&self) -> DefaultSignalAction {
        match self {
            Signo::SIGHUP => DefaultSignalAction::Terminate,
            Signo::SIGINT => DefaultSignalAction::Terminate,
            Signo::SIGQUIT => DefaultSignalAction::CoreDump,
            Signo::SIGILL => DefaultSignalAction::CoreDump,
            Signo::SIGTRAP => DefaultSignalAction::CoreDump,
            Signo::SIGABRT => DefaultSignalAction::CoreDump,
            Signo::SIGBUS => DefaultSignalAction::CoreDump,
            Signo::SIGFPE => DefaultSignalAction::CoreDump,
            Signo::SIGKILL => DefaultSignalAction::Terminate,
            Signo::SIGUSR1 => DefaultSignalAction::Terminate,
            Signo::SIGSEGV => DefaultSignalAction::CoreDump,
            Signo::SIGUSR2 => DefaultSignalAction::Terminate,
            Signo::SIGPIPE => DefaultSignalAction::Terminate,
            Signo::SIGALRM => DefaultSignalAction::Terminate,
            Signo::SIGTERM => DefaultSignalAction::Terminate,
            Signo::SIGSTKFLT => DefaultSignalAction::Terminate,
            Signo::SIGCHLD => DefaultSignalAction::Ignore,
            Signo::SIGCONT => DefaultSignalAction::Continue,
            Signo::SIGSTOP => DefaultSignalAction::Stop,
            Signo::SIGTSTP => DefaultSignalAction::Stop,
            Signo::SIGTTIN => DefaultSignalAction::Stop,
            Signo::SIGTTOU => DefaultSignalAction::Stop,
            Signo::SIGURG => DefaultSignalAction::Ignore,
            Signo::SIGXCPU => DefaultSignalAction::CoreDump,
            Signo::SIGXFSZ => DefaultSignalAction::CoreDump,
            Signo::SIGVTALRM => DefaultSignalAction::Terminate,
            Signo::SIGPROF => DefaultSignalAction::Terminate,
            Signo::SIGWINCH => DefaultSignalAction::Ignore,
            Signo::SIGIO => DefaultSignalAction::Terminate,
            Signo::SIGPWR => DefaultSignalAction::Terminate,
            Signo::SIGSYS => DefaultSignalAction::CoreDump,
            _ if self.is_realtime() => DefaultSignalAction::Terminate,
            _ => DefaultSignalAction::Ignore,
        }
    }
}

/// Signal set. Compatible with `struct sigset_t` in libc.
#[derive(Default, Clone, Copy, Not, BitOr, BitOrAssign, BitAnd, BitAndAssign)]
#[repr(transparent)]
pub struct SignalSet(u64);

impl SignalSet {
    fn signo_bit(signo: Signo) -> u64 {
        1 << (signo as u8 - 1)
    }

    /// Adds a signal to the set.
    pub fn add(&mut self, signal: Signo) -> bool {
        let bit = Self::signo_bit(signal);
        if self.0 & bit != 0 {
            return false;
        }
        self.0 |= bit;
        true
    }

    /// Removes a signal from the set.
    pub fn remove(&mut self, signal: Signo) -> bool {
        let bit = Self::signo_bit(signal);
        if self.0 & bit == 0 {
            return false;
        }
        self.0 &= !bit;
        true
    }

    /// Checks if the set contains a signal.
    pub fn has(&self, signal: Signo) -> bool {
        (self.0 & Self::signo_bit(signal)) != 0
    }

    /// Returns `true` if the set is empty.
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// Dequeues the a signal in `mask` from this set, if any.
    pub fn dequeue(&mut self, mask: &SignalSet) -> Option<Signo> {
        let bits = self.0 & mask.0;
        if bits == 0 {
            None
        } else {
            let signal = bits.trailing_zeros();
            self.0 &= !(1 << signal);
            Signo::from_repr((signal + 1) as u8)
        }
    }
}

impl From<SignalSet> for kernel_sigset_t {
    fn from(value: SignalSet) -> Self {
        k_sigset::from(value).into()
    }
}

impl From<kernel_sigset_t> for SignalSet {
    fn from(value: kernel_sigset_t) -> Self {
        k_sigset::from(value).into()
    }
}

impl From<SignalSet> for k_sigset {
    fn from(value: SignalSet) -> Self {
        Self(value.0)
    }
}

impl From<k_sigset> for SignalSet {
    fn from(value: k_sigset) -> Self {
        Self(value.0)
    }
}

impl fmt::Debug for SignalSet {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let mut debug = f.debug_set();
        for signo in Signo::iter() {
            if self.has(signo) {
                debug.entry(&signo);
            }
        }
        debug.finish()
    }
}

/// Signal information. Compatible with `struct siginfo` in libc.
#[derive(Clone)]
#[repr(transparent)]
pub struct SignalInfo(pub siginfo_t);

impl SignalInfo {
    /// Construct a kernel-originated signal.
    pub fn new_kernel(signo: Signo) -> Self {
        // SAFETY: siginfo_t is a C struct where all-zeroes is a valid
        // representation.  Fields are immediately overwritten below.
        let mut result: Self = unsafe { mem::zeroed() };
        result.set_signo(signo);
        result.set_code(SI_KERNEL as _);
        result
    }

    /// Construct a user-originated signal with a code and pid.
    pub fn new_user(signo: Signo, code: i32, pid: u32) -> Self {
        // SAFETY: siginfo_t is a C struct where all-zeroes is a valid
        // representation.  Fields are immediately overwritten below.
        let mut result: Self = unsafe { mem::zeroed() };
        result.set_signo(signo);
        result.set_code(code);
        result
            .0
            .__bindgen_anon_1
            .__bindgen_anon_1
            ._sifields
            ._sigchld
            ._pid = pid as _;
        result
    }

    /// Construct a timer-originated signal.
    pub fn new_timer(
        signo: Signo,
        timer_id: i32,
        overrun: i32,
        value: k_sigval,
        signal_seq: u32,
    ) -> Self {
        // SAFETY: siginfo_t is a C struct where all-zeroes is a valid
        // representation.  Fields are immediately overwritten below.
        let mut result: Self = unsafe { mem::zeroed() };
        result.set_signo(signo);
        result.set_code(SI_TIMER as _);
        result.set_timer_fields(timer_id, overrun, value, signal_seq);
        result
    }

    /// Returns the signal number.
    pub fn signo(&self) -> Signo {
        // SAFETY: bindgen preserves the union layout; reading si_signo through
        // the anonymous union is a direct field access matching the C ABI.
        unsafe { Signo::from_repr(self.0.__bindgen_anon_1.__bindgen_anon_1.si_signo as _).unwrap() }
    }

    /// Updates the signal number.
    pub fn set_signo(&mut self, signo: Signo) {
        self.0.__bindgen_anon_1.__bindgen_anon_1.si_signo = signo as _;
    }

    /// Returns the signal code.
    pub fn code(&self) -> i32 {
        // SAFETY: bindgen preserves the union layout; si_code occupies the
        // same offset in every union arm.
        unsafe { self.0.__bindgen_anon_1.__bindgen_anon_1.si_code }
    }

    /// Updates the signal code.
    pub fn set_code(&mut self, code: i32) {
        self.0.__bindgen_anon_1.__bindgen_anon_1.si_code = code;
    }

    /// Returns the stored errno value.
    pub fn errno(&self) -> i32 {
        // SAFETY: The union layout matches Linux's siginfo_t definition. bindgen keeps this layout,
        // so it is safe to read the errno field through the anonymous union.
        unsafe { self.0.__bindgen_anon_1.__bindgen_anon_1.si_errno }
    }

    /// Returns the timer ID carried by a `SI_TIMER` signal.
    pub fn timer_id(&self) -> Option<i32> {
        (self.code() == SI_TIMER as _).then_some(unsafe {
            // SAFETY: guarded by SI_TIMER check, meaning the `_timer` union
            // arm was populated by `set_timer_fields`.
            self.0
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._timer
                ._tid
        })
    }

    /// Returns the overrun count carried by a `SI_TIMER` signal.
    pub fn timer_overrun(&self) -> Option<i32> {
        (self.code() == SI_TIMER as _).then_some(unsafe {
            // SAFETY: guarded by SI_TIMER check, meaning the `_timer` union
            // arm was populated by `set_timer_fields`.
            self.0
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._timer
                ._overrun
        })
    }

    /// Returns the timer sequence carried by a `SI_TIMER` signal.
    pub fn timer_signal_seq(&self) -> Option<u32> {
        (self.code() == SI_TIMER as _).then_some(unsafe {
            // SAFETY: guarded by SI_TIMER check, meaning the `_timer` union
            // arm was populated by `set_timer_fields`.
            self.0
                .__bindgen_anon_1
                .__bindgen_anon_1
                ._sifields
                ._timer
                ._sys_private as u32
        })
    }

    /// Returns the `sigval` payload carried by this signal, if present.
    ///
    /// Only `SI_TIMER` and user-originated signals (`code < 0`, e.g. `SI_QUEUE`)
    /// carry a sigval.  Kernel-originated signals (`SI_KERNEL`, positive codes)
    /// do not.
    pub fn sigval(&self) -> Option<k_sigval> {
        match self.code() {
            code if code == SI_TIMER as _ => Some(unsafe {
                // SAFETY: SI_TIMER signals populate the `_timer._sigval` union
                // arm during construction (see `set_timer_fields`).
                self.0
                    .__bindgen_anon_1
                    .__bindgen_anon_1
                    ._sifields
                    ._timer
                    ._sigval
            }),
            code if code < 0 => Some(unsafe {
                // SAFETY: Negative si_code indicates a user-originated signal
                // (SI_QUEUE, SI_MESGQ, SI_ASYNCIO) whose `_rt._sigval` field
                // was populated by the sender.
                self.0
                    .__bindgen_anon_1
                    .__bindgen_anon_1
                    ._sifields
                    ._rt
                    ._sigval
            }),
            _ => None,
        }
    }

    fn set_timer_fields(&mut self, timer_id: i32, overrun: i32, value: k_sigval, signal_seq: u32) {
        self.0
            .__bindgen_anon_1
            .__bindgen_anon_1
            ._sifields
            ._timer
            ._tid = timer_id;
        self.0
            .__bindgen_anon_1
            .__bindgen_anon_1
            ._sifields
            ._timer
            ._overrun = overrun;
        self.0
            .__bindgen_anon_1
            .__bindgen_anon_1
            ._sifields
            ._timer
            ._sigval = value;
        self.0
            .__bindgen_anon_1
            .__bindgen_anon_1
            ._sifields
            ._timer
            ._sys_private = signal_seq as _;
    }
}

unsafe impl Send for SignalInfo {}
unsafe impl Sync for SignalInfo {}

impl fmt::Debug for SignalInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SignalInfo")
            .field("signo", &self.signo())
            .field("code", &self.code())
            .finish()
    }
}

impl From<SignalInfo> for k_siginfo {
    fn from(value: SignalInfo) -> Self {
        Self(value.0)
    }
}

impl From<k_siginfo> for SignalInfo {
    fn from(value: k_siginfo) -> Self {
        Self(value.0)
    }
}

impl From<SignalStack> for k_sigaltstack {
    fn from(value: SignalStack) -> Self {
        Self {
            sp: value.sp,
            flags: value.flags,
            abi_pad: 0,
            size: value.size,
        }
    }
}

impl From<k_sigaltstack> for SignalStack {
    fn from(value: k_sigaltstack) -> Self {
        Self {
            sp: value.sp,
            flags: value.flags,
            size: value.size,
        }
    }
}

/// Signal handler stack configuration.
#[derive(Clone)]
pub struct SignalStack {
    pub sp: usize,
    pub flags: u32,
    pub size: usize,
}

impl Default for SignalStack {
    fn default() -> Self {
        Self {
            sp: 0,
            flags: SS_DISABLE,
            size: 0,
        }
    }
}

impl SignalStack {
    /// Checks if signal stack is disabled.
    pub fn disabled(&self) -> bool {
        self.flags == SS_DISABLE
    }
}
