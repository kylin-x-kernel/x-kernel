// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use ktime_types::{MonotonicInstant, SystemTime, TimeSpan};
use unittest::{assert, assert_eq, def_test};

use super::*;

#[def_test]
fn evdev_strings_follow_linux_truncation_and_nul_rules() {
    assert_eq!(evdev_string_bytes("key", 0), b"");
    assert_eq!(evdev_string_bytes("key", 2), b"ke");
    assert_eq!(evdev_string_bytes("key", 3), b"key");
    assert_eq!(evdev_string_bytes("key", 4), b"key\0");
    assert_eq!(evdev_string_bytes("key", 8), b"key\0");
}

fn frame() -> [TimedInputEvent; 2] {
    let time = SystemTime::from_unix_seconds(1_700_000_000)
        .checked_add(TimeSpan::from_nanos(123_456_789))
        .unwrap();
    let monotonic = MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(45_987_654_321));
    [
        TimedInputEvent {
            time,
            monotonic,
            event: Event::new(EventType::Key as u16, 30, 1),
        },
        TimedInputEvent {
            time,
            monotonic,
            event: Event::new(EventType::Synchronization as u16, SYN_REPORT, 0),
        },
    ]
}

#[def_test]
fn independent_event_clients_receive_the_same_frame() {
    let first = EventClient::new();
    let second = EventClient::new();
    first.receive_frame(&frame());
    second.receive_frame(&frame());

    let mut first_buf = [0u8; size_of::<InputEvent>() * 2];
    let mut second_buf = [0u8; size_of::<InputEvent>() * 2];
    assert_eq!(first.read(&mut first_buf), Ok(first_buf.len()));
    assert_eq!(second.read(&mut second_buf), Ok(second_buf.len()));
    assert_eq!(first_buf, second_buf);
    assert_eq!(first.read(&mut first_buf), Err(KError::WouldBlock));
}

#[def_test]
fn slow_event_client_reports_overflow_without_affecting_peer() {
    let slow = EventClient::new();
    let fast = EventClient::new();
    let input_frame = frame();

    for _ in 0..MAX_CLIENT_EVENTS / input_frame.len() {
        slow.receive_frame(&input_frame);
    }
    fast.receive_frame(&input_frame);
    slow.receive_frame(&input_frame);

    let slow_state = slow.state.lock();
    assert_eq!(slow_state.events.len(), 2);
    assert_eq!(slow_state.events[0].code, SYN_DROPPED);
    assert_eq!(slow_state.events[1].code, SYN_REPORT);
    drop(slow_state);
    assert_eq!(fast.state.lock().events.len(), input_frame.len());
    assert!(fast.poll().contains(IoEvents::IN));
}

#[def_test]
fn event_clock_selection_is_per_client_and_cached() {
    let realtime = EventClient::new();
    let monotonic = Arc::new(EventClient::new());
    // Duplicated VfsFiles share this same client; actual dup is a guest test.
    let shared = monotonic.clone();
    let boottime = EventClient::new();
    assert_eq!(shared.set_clock_id(CLOCK_MONOTONIC as i32), Ok(()));
    assert_eq!(boottime.set_clock_id(CLOCK_BOOTTIME as i32), Ok(()));
    let captured = frame();
    realtime.receive_frame(&captured);
    monotonic.receive_frame(&captured);
    boottime.receive_frame(&captured);

    // These fixed capture times differ from the time at which read runs.
    // Re-stamping during read, or converting from the current wall clock,
    // would therefore violate the recorded event time.
    for (client, seconds, micros) in [
        (&realtime, 1_700_000_000, 123_456),
        (shared.as_ref(), 45, 987_654),
        (&boottime, 45, 987_654),
    ] {
        let mut buf = [0u8; size_of::<InputEvent>()];
        assert_eq!(client.read(&mut buf), Ok(buf.len()));
        let event = InputEvent::read_from_bytes(&buf).unwrap();
        assert_eq!(event.time.tv_sec, seconds);
        assert_eq!(event.time.tv_usec, micros);
        assert_eq!(event.code, 30);
    }
    // Reading through the shared handle consumed the same queue.
    assert_eq!(monotonic.state.lock().events.len(), 1);
}

#[def_test]
fn invalid_event_clock_ids_preserve_state_and_queue() {
    let client = EventClient::new();
    client.receive_frame(&frame());
    for clock_id in [-1, 2, 3, 4, 5, 6, 8, 9, i32::MAX] {
        assert_eq!(client.set_clock_id(clock_id), Err(KError::InvalidInput));
    }
    let state = client.state.lock();
    assert_eq!(state.clock, EventClock::Realtime);
    assert_eq!(state.events.len(), 2);
    assert!(!state.is_waiting_for_report);
    assert_eq!(state.events[0].code, 30);
    assert_eq!(state.events[0].time.tv_sec, 1_700_000_000);
}

#[def_test]
fn setting_same_event_clock_preserves_partially_read_frame() {
    let client = EventClient::new();
    client.receive_frame(&frame());
    let mut buf = [0u8; size_of::<InputEvent>()];
    assert_eq!(client.read(&mut buf), Ok(buf.len()));
    assert_eq!(InputEvent::read_from_bytes(&buf).unwrap().code, 30);
    assert!(client.poll().contains(IoEvents::IN));
    assert_eq!(client.set_clock_id(CLOCK_REALTIME as i32), Ok(()));
    assert_eq!(client.read(&mut buf), Ok(buf.len()));
    let report = InputEvent::read_from_bytes(&buf).unwrap();
    assert_eq!(report.code, SYN_REPORT);
    assert_eq!(report.time.tv_sec, 1_700_000_000);
    assert!(client.poll().is_empty());
}

#[def_test]
fn changing_event_clock_waits_for_report_after_discarding_queue() {
    let changed = EventClient::new();
    let peer = EventClient::new();
    let captured = frame();
    changed.receive_frame(&captured);
    peer.receive_frame(&captured);
    let mut one = [0u8; size_of::<InputEvent>()];
    assert_eq!(changed.read(&mut one), Ok(one.len()));
    assert_eq!(changed.set_clock_id(CLOCK_MONOTONIC as i32), Ok(()));
    assert!(changed.poll().is_empty());
    assert_eq!(changed.read(&mut one), Err(KError::WouldBlock));
    changed.receive_frame(&[]);
    assert!(changed.poll().is_empty());
    assert_eq!(peer.state.lock().events.len(), 2);
    assert!(peer.poll().contains(IoEvents::IN));

    // A real synchronization report completes the pending drop packet.
    changed.receive_frame(&captured[1..]);
    let mut two = [0u8; size_of::<InputEvent>() * 2];
    assert_eq!(changed.read(&mut two), Ok(two.len()));
    let dropped = InputEvent::read_from_bytes(&two[..one.len()]).unwrap();
    let report = InputEvent::read_from_bytes(&two[one.len()..]).unwrap();
    assert_eq!(dropped.event_type, EventType::Synchronization as u16);
    assert_eq!(dropped.code, SYN_DROPPED);
    assert_eq!(report.code, SYN_REPORT);
    assert_eq!(report.time.tv_sec, 45);
    assert_eq!(report.time.tv_usec, 987_654);
    assert!(changed.poll().is_empty());
}

#[def_test]
fn repeated_event_clock_changes_keep_one_pending_drop() {
    let client = EventClient::new();
    client.receive_frame(&frame());
    assert_eq!(client.set_clock_id(CLOCK_MONOTONIC as i32), Ok(()));
    assert_eq!(client.set_clock_id(CLOCK_BOOTTIME as i32), Ok(()));
    let before = client.state.lock().events[0].as_bytes().to_vec();
    assert_eq!(client.set_clock_id(CLOCK_BOOTTIME as i32), Ok(()));
    let state = client.state.lock();
    assert_eq!(state.clock, EventClock::Boottime);
    assert_eq!(state.events.len(), 1);
    assert_eq!(state.events[0].code, SYN_DROPPED);
    assert_eq!(state.events[0].as_bytes(), before.as_slice());
    assert!(state.is_waiting_for_report);
}

#[def_test]
fn changing_clock_on_empty_event_queue_does_not_report_loss() {
    let client = EventClient::new();
    for clock in [CLOCK_MONOTONIC, CLOCK_BOOTTIME, CLOCK_REALTIME] {
        assert_eq!(client.set_clock_id(clock as i32), Ok(()));
        assert!(client.state.lock().events.is_empty());
        assert!(!client.state.lock().is_waiting_for_report);
        assert!(client.poll().is_empty());
    }
    let mut buf = [0u8; size_of::<InputEvent>()];
    assert_eq!(client.read(&mut buf), Err(KError::WouldBlock));
}

#[def_test]
fn overflow_completes_pending_clock_drop_in_selected_domain() {
    let client = EventClient::new();
    client.receive_frame(&frame());
    assert_eq!(client.set_clock_id(CLOCK_MONOTONIC as i32), Ok(()));
    let captured = frame();
    let mut full = [captured[0]; MAX_CLIENT_EVENTS];
    full[MAX_CLIENT_EVENTS - 1] = captured[1];
    client.receive_frame(&full);
    assert!(client.poll().contains(IoEvents::IN));
    let mut buf = [0u8; size_of::<InputEvent>() * 2];
    assert_eq!(client.read(&mut buf), Ok(buf.len()));
    for (record, code) in buf
        .chunks_exact(size_of::<InputEvent>())
        .zip([SYN_DROPPED, SYN_REPORT])
    {
        let event = InputEvent::read_from_bytes(record).unwrap();
        assert_eq!(event.code, code);
        assert_eq!(event.time.tv_sec, 45);
        assert_eq!(event.time.tv_usec, 987_654);
    }
    assert!(client.poll().is_empty());
}

#[def_test]
fn clock_pending_state_preserves_read_size_and_detach_contracts() {
    let client = EventClient::new();
    let mut short = [0u8; size_of::<InputEvent>() - 1];
    assert_eq!(client.read(&mut []), Ok(0));
    assert_eq!(client.read(&mut short), Err(KError::InvalidInput));
    client.receive_frame(&frame());
    assert_eq!(client.set_clock_id(CLOCK_MONOTONIC as i32), Ok(()));
    assert_eq!(client.read(&mut []), Ok(0));
    assert_eq!(client.read(&mut short), Err(KError::InvalidInput));
    client.detach();
    assert_eq!(
        client.set_clock_id(CLOCK_REALTIME as i32),
        Err(KError::NoSuchDevice)
    );
    assert_eq!(client.read(&mut []), Err(KError::NoSuchDevice));
    assert!(client.poll().contains(IoEvents::ERR | IoEvents::HUP));
    client.receive_frame(&frame());
    assert!(client.state.lock().events.is_empty());
}
