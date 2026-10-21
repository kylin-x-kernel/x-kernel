// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime integration probe using the permanent boot-created service.
//! This only observes IRQ progress; the timeout is a hang bound, not a
//! latency requirement. No global registration is installed by the test.

use unittest::{assert, def_test};

use super::*;

#[def_test(serial)]
fn network_sampling_repeats_from_real_timer_irq() {
    let before = SAMPLE_COUNT.load(Ordering::Acquire);
    let limit = monotonic_time() + TimeSpan::from_secs(2);
    while SAMPLE_COUNT.load(Ordering::Acquire) < before + 3 {
        assert!(monotonic_time() < limit);
        ktask::sleep(TimeSpan::from_millis(10));
    }
}
