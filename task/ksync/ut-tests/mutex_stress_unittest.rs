// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::sync::atomic::{AtomicU8, Ordering};

use ktask as thread;
use unittest::{assert_eq, def_test};

static INTERRUPT_TICK: AtomicU8 = AtomicU8::new(0);

fn may_interrupt() {
    // Deterministically model occasional interruption without adding a host-test dependency.
    if INTERRUPT_TICK
        .fetch_add(1, Ordering::Relaxed)
        .is_multiple_of(3)
    {
        thread::yield_now();
    }
}

#[def_test]
fn lots_and_lots() {
    const NUM_TASKS: u32 = 10;
    const NUM_ITERS: u32 = 10_000;
    crate::static_lock! {
        static M: crate::Mutex<u32> = crate::Mutex::new(0);
    }

    fn inc(delta: u32) {
        for _ in 0..NUM_ITERS {
            let mut val = M.lock();
            *val += delta;
            may_interrupt();
            drop(val);
            may_interrupt();
        }
    }

    for _ in 0..NUM_TASKS {
        thread::spawn(|| inc(1));
        thread::spawn(|| inc(2));
    }

    loop {
        let val = M.lock();
        if *val == NUM_ITERS * NUM_TASKS * 3 {
            break;
        }
        may_interrupt();
        drop(val);
        may_interrupt();
    }

    assert_eq!(*M.lock(), NUM_ITERS * NUM_TASKS * 3);
}
