// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Timer-domain delivery descriptions.

use ksignal::{SignalNotification, Signo, TimerSignalTarget};

use crate::Tid;

/// A timer-produced signal before it is converted into `SignalInfo`.
///
/// Not Clone: one expiration handoff must not mint multiple dequeue receipts.
#[expect(
    clippy::large_enum_variant,
    reason = "CG-024-lint-exceptions: clippy::large_enum_variant: POSIX handoff keeps the unique \
              reserved-record receipt inline to avoid a new fallible allocation after expiration"
)]
pub enum TimerSignal {
    /// Legacy `setitimer` expiration (`SIGALRM`/`SIGVTALRM`/`SIGPROF`)
    /// carrying no payload.
    Legacy {
        /// Signal number implied by the interval-timer kind.
        signo: Signo,
    },
    /// POSIX timer expiration carrying the `SI_TIMER` payload fields.
    Posix {
        /// Stable destination; the outer numeric routing fields are not authority.
        target: TimerSignalTarget,
        /// Unique lease of the record reserved at creation, with immutable payload.
        notification: SignalNotification,
    },
}

/// A process- or thread-directed timer delivery.
///
/// Consumed once by dispatch; timer-origin ownership cannot be duplicated here.
pub enum TimerDelivery {
    /// Deliver to the process (any unblocked thread may handle it).
    Process(TimerSignal),
    /// Deliver to one specific thread (`SIGEV_THREAD_ID`).
    Thread {
        /// Requested thread ID; POSIX dispatch uses the signal's captured target.
        tid: Tid,
        /// Notification signal payload.
        signal: TimerSignal,
    },
}
