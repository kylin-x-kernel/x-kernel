// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use lock_api::RawMutex;
use unittest::def_test;

use super::TraceRawLock;

#[def_test]
fn trace_ring_retains_newest_records_without_growing_or_dropping_payloads() {
    let mut ring = super::TraceRing::new();
    ring.reserve();
    let capacity = ring.records.capacity();
    let allocation = ring.records.make_contiguous().as_ptr();
    let name = *b"trace-ring-test\0";
    for sequence in 0..super::TRACE_RECORD_CAPACITY + 7 {
        ring.push(&(sequence as u64).to_ne_bytes(), sequence as u32, name);
        assert_eq!(ring.records.capacity(), capacity);
    }
    assert_eq!(ring.stats().retained_records, super::TRACE_RECORD_CAPACITY);
    assert_eq!(ring.stats().overwritten_records, 7);
    assert_eq!(ring.stats().oversized_records, 0);
    assert_eq!(ring.stats().uninitialized_records, 0);
    assert!(!core::mem::needs_drop::<super::TraceRecord>());
    assert_eq!(ring.records.make_contiguous().as_ptr(), allocation);
    for (offset, record) in ring.records.iter().enumerate() {
        let sequence = offset + 7;
        assert_eq!(
            &record.bytes[..record.length],
            &(sequence as u64).to_ne_bytes()
        );
        assert_eq!(record.pid, sequence as u32);
        assert_eq!(record.name, name);
    }
}

#[def_test]
fn trace_ring_counts_rejection_without_evicting_valid_records() {
    let mut ring = super::TraceRing::new();
    ring.push(&[1], 1, [0; super::TASK_NAME_BYTES]);
    assert_eq!(ring.stats().uninitialized_records, 1);
    assert!(ring.records.is_empty());
    ring.reserve();
    let capacity = ring.records.capacity();
    for _ in 0..super::TRACE_RECORD_CAPACITY {
        ring.push(
            &[2; super::MAX_RECORD_BYTES],
            2,
            [0; super::TASK_NAME_BYTES],
        );
    }
    ring.push(
        &[3; super::MAX_RECORD_BYTES + 1],
        3,
        [0; super::TASK_NAME_BYTES],
    );
    assert_eq!(ring.stats().oversized_records, 1);
    assert_eq!(ring.stats().overwritten_records, 0);
    assert_eq!(
        alloc::format!("{}", ring.stats()),
        alloc::format!(
            "retained_records: {}\noverwritten_records: 0\noversized_records: 1\nuninitialized_records: 1\n",
            super::TRACE_RECORD_CAPACITY
        )
    );
    assert_eq!(ring.records.capacity(), capacity);
    assert!(ring.records.iter().all(|record| record.pid == 2));
    ring.oversized_records = usize::MAX;
    ring.push(
        &[3; super::MAX_RECORD_BYTES + 1],
        3,
        [0; super::TASK_NAME_BYTES],
    );
    assert_eq!(ring.stats().oversized_records, usize::MAX);
}

#[def_test]
fn task_name_copy_preserves_utf8_boundaries_and_unused_bytes() {
    let name = "aé🦀z";
    let task = ktask::TaskInner::new_pidless_kthread(|| {}, name.into(), 0x4000);
    for capacity in 0..=12 {
        let mut buffer = [0xaa; 12];
        let copied = task.copy_name(&mut buffer[..capacity]);
        let expected = (0..=name.len().min(capacity))
            .rev()
            .find(|length| name.is_char_boundary(*length))
            .unwrap();
        assert_eq!(copied, expected);
        assert_eq!(&buffer[..copied], &name.as_bytes()[..expected]);
        assert!(core::str::from_utf8(&buffer[..copied]).is_ok());
        assert!(buffer[copied..].iter().all(|byte| *byte == 0xaa));
    }
}

#[def_test(serial)]
fn captured_scheduler_records_preserve_payloads_and_per_record_names() {
    let manager = super::trace_point_manager();
    let map = manager.tracepoint_map();
    let mut ring = super::TraceRing::new();
    ring.reserve();
    let task = ktask::TaskInner::new_pidless_kthread(|| {}, "first-name".into(), 0x4000);
    for (event, values, name) in [
        ("sched_wakeup", &[17u64, 123456789][..], "first-name"),
        (
            "sched_switch",
            &[17u64, 23, 3, 987654321][..],
            "renamed-é🦀",
        ),
    ] {
        task.set_name(name);
        let mut captured_name = [0; super::TASK_NAME_BYTES];
        task.copy_name(&mut captured_name);
        let id: u16 = super::event_id("sched", event)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        let mut bytes = [0; super::MAX_RECORD_BYTES];
        bytes[..2].copy_from_slice(&id.to_ne_bytes());
        bytes[4..8].copy_from_slice(&71i32.to_ne_bytes());
        for (index, value) in values.iter().enumerate() {
            let start = 8 + index * 8;
            bytes[start..start + 8].copy_from_slice(&value.to_ne_bytes());
        }
        ring.push(&bytes[..8 + values.len() * 8], 71, captured_name);
    }
    let mut cache = ktracepoint::TraceCmdLineCache::new(1);
    let mut parsed = alloc::vec::Vec::new();
    for record in &ring.records {
        assert_eq!(record.bytes.as_ptr() as usize % 8, 0);
        cache.insert(
            record.pid,
            alloc::string::String::from_utf8_lossy(&record.name).into_owned(),
        );
        parsed.push(ktracepoint::TraceEntryParser::parse::<super::Kops, _>(
            &map,
            &cache,
            &record.bytes[..record.length],
        ));
    }
    assert!(parsed[0].contains("first-name-71"));
    assert!(parsed[0].contains("sched_wakeup(woken_tid=17 ts_ns=123456789)"));
    assert!(parsed[1].contains("renamed-é🦀-71"));
    assert!(
        parsed[1].contains("sched_switch(prev_tid=17 next_tid=23 prev_state=3 ts_ns=987654321)")
    );
}

struct RestoreSchedulerEvents {
    states: [(&'static str, bool, bool); 2],
}

impl RestoreSchedulerEvents {
    fn enable() -> Self {
        let states = ["sched_wakeup", "sched_switch"].map(|name| {
            super::with_event("sched", name, |event| {
                let point = event.tracepoint();
                (name, point.default_is_enabled(), point.event_is_enabled())
            })
            .unwrap()
        });
        let restore = Self { states };
        for (name, ..) in states {
            assert!(super::write_event_enable("sched", name, b"1"));
        }
        restore
    }
}

impl Drop for RestoreSchedulerEvents {
    fn drop(&mut self) {
        for (name, default_enabled, event_enabled) in self.states {
            super::with_event("sched", name, |event| {
                event
                    .enable_file()
                    .write(if default_enabled { '1' } else { '0' });
                if event_enabled {
                    event.tracepoint().enable_event();
                } else {
                    event.tracepoint().disable_event();
                }
            });
        }
    }
}

#[def_test(serial)]
fn enabled_scheduler_trace_survives_completion_wake_from_timer_irq() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    let completion = Arc::new(kcompletion::Completion::new());
    let notified_in_irq = Arc::new(AtomicBool::new(false));
    let timer = ktask::Timer::new({
        let completion = completion.clone();
        let notified_in_irq = notified_in_irq.clone();
        move |_: ktask::TimerControl<'_>, _: ktask::Expiration| {
            notified_in_irq.store(!karch::local_irq_enabled(), Ordering::Release);
            completion.complete_all();
        }
    });
    let restore = RestoreSchedulerEvents::enable();
    let before = super::trace_capture_stats();
    let waiter = ktask::spawn_with_name(
        {
            let completion = completion.clone();
            move || {
                ktask::completion::CompletionWait::prepare()
                    .unwrap()
                    .wait(&completion)
            }
        },
        "trace-completion".into(),
    );
    while waiter.state() != ktask::TaskState::Blocked {
        ktask::yield_now();
    }
    let deadline = khal::time::monotonic_time()
        .checked_add(ktime_types::TimeSpan::from_millis(2))
        .unwrap();
    // SAFETY: the kernel test runs after scheduler/timer initialization. The
    // owned handler only stores a flag and wakes a completion; it neither
    // sleeps, allocates, unwinds nor reenters timer dispatch, and is drained below.
    unsafe { timer.start(deadline) }.unwrap();
    assert_eq!(waiter.join(), 0);
    timer.cancel_sync();
    let after = super::trace_capture_stats();
    let snapshot = super::dump_trace_records();
    let previous_states = restore.states;
    drop(restore);
    for (name, default_enabled, event_enabled) in previous_states {
        let restored = super::with_event("sched", name, |event| {
            let point = event.tracepoint();
            (point.default_is_enabled(), point.event_is_enabled())
        });
        assert_eq!(restored, Some((default_enabled, event_enabled)));
    }
    assert!(notified_in_irq.load(Ordering::Acquire));
    assert_eq!(after.oversized_records, before.oversized_records);
    assert_eq!(after.uninitialized_records, before.uninitialized_records);
    assert!(snapshot.starts_with("# tracer: nop\n"));
    assert!(snapshot.contains(&alloc::format!(
        "sched_wakeup(woken_tid={} ",
        waiter.trace_id()
    )));
    assert!(snapshot.contains(&alloc::format!(
        "sched_switch(prev_tid={} ",
        waiter.trace_id()
    )));
    assert!(snapshot.contains(&alloc::format!("next_tid={} ", waiter.trace_id())));
    assert!(snapshot.contains("trace-completion"));
}

#[def_test]
fn trace_raw_lock_try_lock_tracks_state_and_unlocks() {
    let lock = TraceRawLock::INIT;

    assert!(!lock.is_locked());
    assert!(lock.try_lock());
    assert!(lock.is_locked());
    assert!(!lock.try_lock());

    // SAFETY: the lock above was acquired successfully and has not yet been unlocked.
    unsafe { lock.unlock() };

    assert!(!lock.is_locked());
    assert!(lock.try_lock());
    // SAFETY: the second acquisition above succeeded and is released here.
    unsafe { lock.unlock() };
}

#[def_test]
fn trace_raw_lock_lock_path_matches_try_lock_state() {
    let lock = TraceRawLock::INIT;

    lock.lock();
    assert!(lock.is_locked());
    assert!(!lock.try_lock());

    // SAFETY: `lock()` above acquired the raw lock and intentionally leaked the guard.
    unsafe { lock.unlock() };

    assert!(!lock.is_locked());
}

#[def_test]
fn trace_header_preserves_upstream_columns_and_snapshot_counts() {
    for count in [0, 1, 3] {
        let records = alloc::vec![alloc::vec![]; count];
        assert_eq!(
            super::trace_header(count),
            ktracepoint::TracePipeSnapshot::new(records).default_fmt_str()
        );
    }
}
