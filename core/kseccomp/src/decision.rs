// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! seccomp return-value interpretation and decision precedence.
//!
//! A classic-BPF filter returns a 32-bit `SECCOMP_RET_*` value. This
//! module decodes the raw return value into a labeled [`Action`],
//! interprets an action's precedence (Linux `seccomp_run_filters` picks
//! the highest-precedence action among all chained filters), and exposes
//! the [`Decision`] type that ksyscall applies at syscall entry.

/// Killed the whole process (Linux `SECCOMP_RET_KILL_PROCESS`).
pub(crate) const SECCOMP_RET_KILL_PROCESS: u32 = 0x8000_0000;
/// Killed the calling thread (Linux `SECCOMP_RET_KILL_THREAD`).
pub(crate) const SECCOMP_RET_KILL_THREAD: u32 = 0x0000_0000;
/// Delivered `SIGSYS` with a user callback (`SECCOMP_RET_TRAP`).
pub(crate) const SECCOMP_RET_TRAP: u32 = 0x0003_0000;
/// Returned an errno from the syscall (`SECCOMP_RET_ERRNO`).
pub(crate) const SECCOMP_RET_ERRNO: u32 = 0x0005_0000;
/// User notification (`SECCOMP_RET_USER_NOTIF`).
pub(crate) const SECCOMP_RET_USER_NOTIF: u32 = 0x7fc0_0000;
/// Trace attachment (`SECCOMP_RET_TRACE`).
pub(crate) const SECCOMP_RET_TRACE: u32 = 0x7ff0_0000;
/// Allow with logging (`SECCOMP_RET_LOG`).
pub(crate) const SECCOMP_RET_LOG: u32 = 0x7ffc_0000;
/// Allow the syscall (`SECCOMP_RET_ALLOW`).
pub(crate) const SECCOMP_RET_ALLOW: u32 = 0x7fff_0000;
/// Mask for the action plus its top bit (catches `KILL_PROCESS`).
pub(crate) const SECCOMP_RET_ACTION_FULL: u32 = 0xffff_0000;
/// Mask for the errno/data payload of an action.
pub(crate) const SECCOMP_RET_DATA: u32 = 0x0000_ffff;

/// The labeled interpretation of a raw `SECCOMP_RET_*` value.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Action {
    /// Allow the syscall.
    Allow,
    /// Return the given errno.
    Errno(u32),
    /// Kill the whole process.
    KillProcess,
    /// Kill only the calling thread.
    KillThread,
    /// Request a userspace notification (unimplemented).
    UserNotif,
    /// Trace with a tracer (unimplemented).
    Trace,
    /// Log and allow.
    Log,
    /// A `SECCOMP_RET_TRAP` request, or any value with no recognised action.
    ///
    /// Linux delivers `SIGSYS` for `TRAP` and terminates the thread for an
    /// unrecognised action, so this is treated as the most restrictive result
    /// rather than being ignored.
    Unknown,
}

impl Action {
    /// Decodes a raw `SECCOMP_RET_*` value into an [`Action`].
    ///
    /// The action occupies the upper 16 bits; the low 16 bits carry the errno
    /// payload for `ERRNO`. `KILL_PROCESS` sets the top bit, so the comparison
    /// must use the full-width mask before masking the action off.
    pub fn from_ret(ret: u32) -> Self {
        match ret & SECCOMP_RET_ACTION_FULL {
            SECCOMP_RET_KILL_PROCESS => Action::KillProcess,
            SECCOMP_RET_KILL_THREAD => Action::KillThread,
            SECCOMP_RET_TRAP => Action::Unknown,
            SECCOMP_RET_ERRNO => Action::Errno(ret & SECCOMP_RET_DATA),
            SECCOMP_RET_USER_NOTIF => Action::UserNotif,
            SECCOMP_RET_TRACE => Action::Trace,
            SECCOMP_RET_LOG => Action::Log,
            SECCOMP_RET_ALLOW => Action::Allow,
            _ => Action::Unknown,
        }
    }

    /// When multiple chained filters disagree, Linux keeps the action with the
    /// highest precedence; a lower value wins. `Unknown` is the most
    /// restrictive because the kernel cannot honour it.
    pub fn precedence(self) -> u8 {
        match self {
            Action::Unknown => 0,
            Action::KillProcess => 1,
            Action::KillThread => 2,
            Action::Errno(_) => 3,
            Action::UserNotif => 4,
            Action::Trace => 5,
            Action::Log => 6,
            Action::Allow => 7,
        }
    }
}

impl From<Action> for Decision {
    fn from(action: Action) -> Self {
        match action {
            Action::Allow => Decision::Allow,
            Action::Errno(errno) => Decision::Errno(errno),
            Action::KillProcess => Decision::KillProcess,
            Action::KillThread => Decision::KillThread,
            Action::UserNotif => Decision::UserNotif,
            Action::Trace => Decision::Trace,
            Action::Log => Decision::Allow,
            Action::Unknown => Decision::Unsupported,
        }
    }
}

/// The final decision a syscall must take, derived from a whole filter
/// chain. Anything stricter than "allow" carries the payload of the
/// highest-precedence filter result.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Decision {
    /// The syscall is allowed.
    Allow,
    /// A filter requested a concrete errno.
    Errno(u32),
    /// A filter requested the process be killed with `SIGSYS`.
    KillProcess,
    /// A filter requested the calling thread be killed with `SIGSYS`.
    KillThread,
    /// A filter requested a user-space notification.
    UserNotif,
    /// A filter requested a tracer (not supported).
    Trace,
    /// The action constant is not recognized; the caller must fail closed.
    Unsupported,
}

/// Computes the single highest-precedence decision from all filter
/// results. Empty input is treated as `allow` (no constraint).
pub fn highest_priority(results: impl IntoIterator<Item = Action>) -> Decision {
    results
        .into_iter()
        .min_by_key(|action| action.precedence())
        .map_or(Decision::Allow, Decision::from)
}
