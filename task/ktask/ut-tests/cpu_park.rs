// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test(serial)]
fn remote_event_pause_and_cpu_release_can_be_repeated() {
    for _ in 0..3 {
        let parked = ParkedCpus::acquire(TimeSpan::from_secs(5)).unwrap();
        assert_eq!(parked.remote_cpu_count(), kcpu_id_map::nr_cpus() - 1);
        for participant in &parked.participants {
            assert_eq!(participant.state.acknowledged.load(Ordering::Acquire), 1);
            assert!(participant.work.is_queued());
        }
        khal::time::busy_wait(TimeSpan::from_micros(100));
        drop(parked);
        crate::yield_now();
    }
}
