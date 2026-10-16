// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Runtime integration probes, not timing benchmarks. Boot-created owners
//! are intentionally permanent; this test only observes their IRQ progress.
//! The time bound detects a missing rearm without requiring precise latency.

use core::sync::atomic::Ordering;

use unittest::{assert, def_test};

use super::*;

#[def_test(serial)]
fn softlockup_sampling_repeats_on_every_cpu() {
    let before: alloc::vec::Vec<_> = SAMPLE_COUNTS[..kcpu_id_map::nr_cpus()]
        .iter()
        .map(|count| count.load(Ordering::Acquire))
        .collect();
    let limit = khal::time::monotonic_time() + TimeSpan::from_secs(15);
    loop {
        if before
            .iter()
            .enumerate()
            .all(|(cpu, before)| SAMPLE_COUNTS[cpu].load(Ordering::Acquire) >= before + 2)
        {
            break;
        }
        assert!(khal::time::monotonic_time() < limit);
        ktask::sleep(TimeSpan::from_millis(100));
    }
}
