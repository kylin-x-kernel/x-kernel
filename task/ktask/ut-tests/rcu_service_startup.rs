// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! RCU host-carrier prerequisites, not a test of GP completion or RCU readiness.
//!
//! This kernel integration test uses scheduler/timer services initialized by
//! boot and persistent throughout the test run, not state from earlier tests.
//! The deadline triggers delivery, with no latency assertion. The fixture joins
//! its task and cancels its owned timer; it installs no global hook or service.

use alloc::sync::Arc;
use core::{
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
    task::Waker,
};

use kspin::SpinNoIrq;
use ktime_types::TimeSpan;
use unittest::{assert, assert_eq, def_test};

use crate::{Expiration, Timer, TimerControl, future::PreparedTaskWait};

#[def_test(serial)]
fn rcu_carrier_prepares_in_owner_before_publishing_and_receives_timer_demand() {
    let mut startup = PreparedTaskWait::prepare().unwrap();
    let published = Arc::new(SpinNoIrq::new(None::<Waker>));
    let demand = Arc::new(AtomicBool::new(false));
    let processed = Arc::new(AtomicUsize::new(0));
    let task = crate::spawn({
        let startup_waker = startup.waker();
        let published = published.clone();
        let demand = demand.clone();
        let processed = processed.clone();
        move || {
            // Preparing in the boot task and moving its wait here would violate
            // park's task identity check, even when both run on the same CPU.
            let mut wait = PreparedTaskWait::prepare().unwrap();
            let timer = Timer::new({
                let waker = wait.waker();
                let demand = demand.clone();
                move |_: TimerControl<'_>, _: Expiration| {
                    demand.store(true, Ordering::Release);
                    waker.wake_by_ref();
                }
            });
            *published.lock() = Some(wait.waker());
            startup_waker.wake_by_ref();

            for round in 0..2 {
                // Consume before work, not after: a concurrent new request must
                // remain visible for the next pass. Wakes alone are not work.
                while !demand.swap(false, Ordering::AcqRel) {
                    wait.park();
                }
                processed.fetch_add(1, Ordering::Release);
                if round == 0 {
                    // SAFETY: the kernel test runs after CPU timer service
                    // initialization. The handler only publishes demand and
                    // wakes outside scheduler/RCU locks; it never cancels itself.
                    unsafe { timer.start(khal::time::monotonic_time() + TimeSpan::from_millis(2)) }
                        .unwrap();
                }
            }
            timer.cancel_sync();
        }
    });
    let waker = loop {
        let waker = published.lock().clone();
        if let Some(waker) = waker {
            break waker;
        }
        // Sleeping, rather than spinning, also permits initialization on UP.
        startup.park();
    };
    let before_request = processed.load(Ordering::Acquire);
    demand.store(true, Ordering::Release);
    waker.wake_by_ref();
    let exit = task.join();
    assert_eq!(exit, 0);
    assert_eq!(before_request, 0);
    assert_eq!(processed.load(Ordering::Acquire), 2);
    assert!(!demand.load(Ordering::Acquire));
}
