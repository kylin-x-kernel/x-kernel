// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

fn instant_secs(secs: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(secs))
}

#[def_test]
fn lockup_state_uses_shared_cache_alignment() {
    let states = [LockupDetection::new(), LockupDetection::new()];
    let first = core::ptr::from_ref(&states[0]).addr();
    let second = core::ptr::from_ref(&states[1]).addr();
    let alignment = karch::cache::SMP_CACHE_BYTES;
    assert_eq!(first % alignment, 0);
    assert_eq!(second % alignment, 0);
    assert!(size_of::<LockupDetection>().is_multiple_of(alignment));
    assert!((first + size_of::<LockupDetection>() - 1) / alignment < second / alignment);
}

#[def_test]
fn hardlockup_uses_wall_time_not_nmi_spacing() {
    let det = LockupDetection::new();
    let t0 = instant_secs(1);
    assert!(!det.check_hardlockup(t0, DEFAULT_HARDLOCKUP_THRESHOLD));

    det.timer_tick(t0);
    assert!(!det.check_hardlockup(instant_secs(5), DEFAULT_HARDLOCKUP_THRESHOLD));
    assert!(!det.check_hardlockup(instant_secs(11), DEFAULT_HARDLOCKUP_THRESHOLD));
    assert!(det.check_hardlockup(instant_secs(12), DEFAULT_HARDLOCKUP_THRESHOLD));
}
