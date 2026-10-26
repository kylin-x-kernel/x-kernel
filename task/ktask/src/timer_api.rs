// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Scoped timer capability backed by the task timer service.

use kerrno::KResult;
use ktime_types::{MonotonicInstant, TimeSpan};

use crate::{Expiration, Timer, TimerControl, TimerHandler};

struct ScopedTimer<H: TimerHandler + 'static> {
    timer: Timer<H>,
}

impl<H: TimerHandler + 'static> kiface_time_api::OneShotTimer for ScopedTimer<H> {
    unsafe fn arm(&mut self, deadline_ns: u64) -> KResult<()> {
        // SAFETY: the adapter caller guarantees initialized scheduler/timer
        // services and IRQ-compatible expiry. The owned timer retains stable
        // callback storage and revalidates routing if the task migrated.
        unsafe {
            self.timer.start(MonotonicInstant::from_span_since_origin(
                TimeSpan::from_nanos(deadline_ns),
            ))
        }
    }

    fn cancel_sync(&mut self) {
        self.timer.cancel_sync();
    }
}

#[kiface::provide]
impl kiface_time_api::ScopedOneShotTimerIf {
    fn with_timer(
        notify_fn: fn(),
        body_fn: &mut dyn FnMut(&mut dyn kiface_time_api::OneShotTimer),
    ) {
        let mut alarm = ScopedTimer {
            timer: Timer::new(move |_: TimerControl<'_>, _: Expiration| notify_fn()),
        };
        body_fn(&mut alarm);
        // Timer's Drop synchronously drains delivery, including on body return.
    }
}
