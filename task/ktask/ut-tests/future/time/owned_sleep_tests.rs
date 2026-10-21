// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{sync::Arc, task::Wake};
use core::sync::atomic::{AtomicUsize, Ordering};

use unittest::{assert, assert_eq, def_test};

use super::*;

struct WakeCount(AtomicUsize);

impl Wake for WakeCount {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

#[def_test(serial)]
fn original_clock_is_checked_on_every_poll() {
    let due = core::cell::Cell::new(false);
    let mut future = core::pin::pin!(sleep_until_clock(
        ktime::ClockDomain::Boottime,
        || due.get(),
        || monotonic_time() + TimeSpan::from_secs(60),
    ));
    let mut cx = Context::from_waker(Waker::noop());
    assert!(future.as_mut().poll(&mut cx).is_pending());
    // No clock notification or core timer expiration: a poll must still
    // observe the original predicate, not rely on the converted timer.
    due.set(true);
    assert!(matches!(future.as_mut().poll(&mut cx), Poll::Ready(Ok(()))));
}

#[def_test(serial)]
fn ready_operation_wins_at_expired_timeout() {
    assert_eq!(
        crate::future::block_on(timeout_at(
            Some(MonotonicInstant::ORIGIN),
            core::future::ready(17),
        )),
        Ok(17)
    );
    assert!(
        crate::future::block_on(timeout_at(
            Some(MonotonicInstant::ORIGIN),
            core::future::pending::<()>(),
        ))
        .is_err()
    );
}

#[def_test(serial)]
fn real_irq_completes_before_first_poll() {
    let _pin = kspin::NoPreempt::new();
    let mut future = TimerFuture::new(monotonic_time() + TimeSpan::from_millis(1)).unwrap();
    let limit = monotonic_time() + TimeSpan::from_secs(2);
    while !future.timer.handler().0.lock().is_expired && monotonic_time() < limit {
        core::hint::spin_loop();
    }
    future.timer.cancel_sync();
    assert!(
        Pin::new(&mut future)
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_ready()
    );
}

#[def_test(serial)]
fn oversized_sleep_stays_pending_but_core_input_is_rejected() {
    let deadline = MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(u64::MAX));
    assert!(TimerFuture::new(deadline).is_err());
    let mut wait = core::pin::pin!(sleep_until(deadline));
    assert!(
        wait.as_mut()
            .poll(&mut Context::from_waker(Waker::noop()))
            .is_pending()
    );
}

#[def_test(serial)]
fn expired_before_poll_completes_without_waking() {
    let mut future = TimerFuture::new(MonotonicInstant::ORIGIN).unwrap();
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    assert!(
        Pin::new(&mut future)
            .poll(&mut Context::from_waker(&waker))
            .is_ready()
    );
    assert_eq!(count.0.load(Ordering::Relaxed), 0);
    assert!(future.timer.handler().0.lock().waker.is_none());
}

#[def_test(serial)]
fn completion_notifies_latest_waker_and_poll_stays_ready() {
    let mut future = TimerFuture::new(monotonic_time() + TimeSpan::from_secs(60)).unwrap();
    future.timer.cancel_sync();
    let first = Arc::new(WakeCount(AtomicUsize::new(0)));
    let second = Arc::new(WakeCount(AtomicUsize::new(0)));
    let first_waker = Waker::from(first.clone());
    let second_waker = Waker::from(second.clone());
    assert!(
        Pin::new(&mut future)
            .poll(&mut Context::from_waker(&first_waker))
            .is_pending()
    );
    assert!(
        Pin::new(&mut future)
            .poll(&mut Context::from_waker(&second_waker))
            .is_pending()
    );
    // Model completion after replacing the registered waker. Actual IRQ
    // delivery is exercised by the subtick sleep/timeout tests below.
    let wake = future.timer.handler().0.lock().complete().unwrap();
    wake.wake();
    assert_eq!(first.0.load(Ordering::Relaxed), 0);
    assert_eq!(second.0.load(Ordering::Relaxed), 1);
    assert!(
        Pin::new(&mut future)
            .poll(&mut Context::from_waker(&second_waker))
            .is_ready()
    );
    assert!(future.timer.handler().0.lock().complete().is_none());
}

#[def_test(serial)]
fn drop_pending_sleep_releases_callback_and_waker() {
    let mut future = TimerFuture::new(monotonic_time() + TimeSpan::from_secs(60)).unwrap();
    let count = Arc::new(WakeCount(AtomicUsize::new(0)));
    let waker = Waker::from(count.clone());
    assert!(
        Pin::new(&mut future)
            .poll(&mut Context::from_waker(&waker))
            .is_pending()
    );
    drop(future);
    assert_eq!(Arc::strong_count(&count), 2);
    assert_eq!(count.0.load(Ordering::Relaxed), 0);
}
