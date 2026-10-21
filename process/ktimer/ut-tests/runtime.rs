// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{BoottimeInstant, SystemTime, TimeSpan};
use unittest::{assert, assert_eq, def_test};

use super::*;

fn mono(seconds: u64) -> MonotonicInstant {
    MonotonicInstant::from_span_since_origin(TimeSpan::from_secs(seconds))
}
#[def_test]
fn offset_change_reorders_domains_without_reordering_heaps() {
    let mut queues = AlarmQueues::default();
    let monotonic = TimerInstant::Monotonic(mono(50));
    let boottime = TimerInstant::Boottime(BoottimeInstant::from_span_since_origin(
        TimeSpan::from_secs(80),
    ));
    let realtime = TimerInstant::Realtime(SystemTime::from_unix_seconds(1000));
    for (pid, deadline) in [(1, monotonic), (2, boottime), (3, realtime)] {
        queues.queue(deadline).unwrap().push(Entry {
            deadline,
            pid,
            ticket: Arc::new(AlarmTicket::new()),
        });
    }
    let convert = |boot_offset: u64, real_offset: u64| {
        queues
            .earliest_with(|deadline| match deadline {
                TimerInstant::Monotonic(deadline) => Some(deadline),
                TimerInstant::Boottime(_) => Some(mono(80 - boot_offset)),
                TimerInstant::Realtime(_) => Some(mono(1000 - real_offset)),
                _ => None,
            })
            .unwrap()
            .1
    };
    assert_eq!(convert(0, 900), monotonic);
    assert_eq!(convert(60, 900), boottime);
    assert_eq!(convert(0, 990), realtime);
    assert_eq!(queues.boottime.peek().unwrap().deadline, boottime);
    assert!(
        queues
            .queue(TimerInstant::ProcessCpu(
                ktime_types::ProcessCpuInstant::ORIGIN
            ))
            .is_none()
    );
}
