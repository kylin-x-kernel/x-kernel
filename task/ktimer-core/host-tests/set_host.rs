// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[test]
fn cpu_slots_follow_shared_cache_alignment() {
    let timers = TimerSet::<4>::new();
    for pair in timers.cpus.windows(2) {
        let first = core::ptr::from_ref(&pair[0]).addr();
        let next = core::ptr::from_ref(&pair[1]).addr();
        assert_eq!(first % SMP_CACHE_BYTES, 0);
        assert_eq!(next - first, size_of_val(&pair[0]));
        assert!((first + size_of_val(&pair[0]) - 1) / SMP_CACHE_BYTES < next / SMP_CACHE_BYTES);
    }
}
