// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Conversion and comparison of protocol deadlines.

use khal::time::monotonic_time;
use ktime_types::MonotonicInstant;
#[cfg(unittest)]
use ktime_types::TimeSpan;
use smoltcp::time::Instant as SmoltcpInstant;

pub(crate) fn to_smoltcp_instant(instant: MonotonicInstant) -> SmoltcpInstant {
    let micros = instant.span_since_origin().as_micros();
    let micros = i64::try_from(micros).unwrap_or(i64::MAX);
    SmoltcpInstant::from_micros_const(micros)
}

#[cfg(unittest)]
pub(crate) fn from_smoltcp_instant(instant: SmoltcpInstant) -> MonotonicInstant {
    let micros = u64::try_from(instant.total_micros()).unwrap_or(0);
    MonotonicInstant::from_span_since_origin(TimeSpan::from_micros(micros))
}

pub(crate) fn now() -> SmoltcpInstant {
    to_smoltcp_instant(monotonic_time())
}

pub(crate) fn has_immediate_timer_work(
    current: SmoltcpInstant,
    next_poll: Option<SmoltcpInstant>,
) -> bool {
    next_poll.is_some_and(|deadline| deadline <= current)
}

pub(crate) fn earliest_poll_deadline(
    protocol: Option<SmoltcpInstant>,
    deferred_close: Option<SmoltcpInstant>,
) -> Option<SmoltcpInstant> {
    [protocol, deferred_close].into_iter().flatten().min()
}

unittest::path_mod_test!(tests, "../../ut-tests/stack/time/tests.rs");
