// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Timer-domain delivery descriptions.

use ksignal::Signo;
use posix_types::k_sigval;

use crate::Tid;

/// A timer-produced signal before it is converted into `SignalInfo`.
#[derive(Clone)]
pub enum TimerSignal {
    Legacy {
        signo: Signo,
    },
    Posix {
        signo: Signo,
        timer_id: i32,
        overrun: i32,
        signal_seq: u32,
        value: k_sigval,
    },
}

/// A process- or thread-directed timer delivery.
#[derive(Clone)]
pub enum TimerDelivery {
    Process(TimerSignal),
    Thread { tid: Tid, signal: TimerSignal },
}
