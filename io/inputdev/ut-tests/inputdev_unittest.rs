// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert, assert_eq, def_test};

use super::*;

struct RecordingListener {
    frames: Mutex<Vec<Vec<TimedInputEvent>>>,
}

impl InputEventListener for RecordingListener {
    fn receive_frame(&self, frame: &[TimedInputEvent]) {
        self.frames.lock().push(frame.to_vec());
    }

    fn detach(&self) {}
}

fn timed(event_type: u16, code: u16, value: u32) -> TimedInputEvent {
    TimedInputEvent {
        time: SystemTime::UNIX_EPOCH,
        monotonic: MonotonicInstant::ORIGIN,
        event: Event::new(event_type, code, value),
    }
}

fn report() -> TimedInputEvent {
    timed(EventType::Synchronization as u16, SYN_REPORT, 0)
}

#[def_test]
fn input_frames_are_copied_to_each_listener() {
    let mut state = EndpointState::new();
    let first = Arc::new(RecordingListener {
        frames: Mutex::new(Vec::new()),
    });
    let second = Arc::new(RecordingListener {
        frames: Mutex::new(Vec::new()),
    });
    let first_listener: Arc<dyn InputEventListener> = first.clone();
    let second_listener: Arc<dyn InputEventListener> = second.clone();
    state.listeners.push(Arc::downgrade(&first_listener));
    state.listeners.push(Arc::downgrade(&second_listener));

    assert!(state.accept(timed(EventType::Key as u16, 30, 1)).is_none());
    let (frame, listeners) = state.accept(report()).unwrap();
    for listener in listeners {
        listener.receive_frame(&frame);
    }

    let first_frames = first.frames.lock();
    let second_frames = second.frames.lock();
    assert_eq!(first_frames[0].as_slice(), frame.as_slice());
    assert_eq!(second_frames[0].as_slice(), frame.as_slice());
    assert_eq!(frame.len(), 2);
    assert!(state.key_state.get(30));
}

#[def_test]
fn listener_added_mid_frame_starts_with_next_report() {
    let mut state = EndpointState::new();
    let listener = Arc::new(RecordingListener {
        frames: Mutex::new(Vec::new()),
    });
    let subscriber: Arc<dyn InputEventListener> = listener.clone();

    assert!(state.accept(timed(EventType::Key as u16, 30, 1)).is_none());
    state.add_listener(&subscriber);
    let (old_frame, old_listeners) = state.accept(report()).unwrap();
    assert_eq!(old_frame.len(), 0);
    assert_eq!(old_listeners.len(), 0);

    assert!(state.accept(timed(EventType::Key as u16, 30, 0)).is_none());
    let (new_frame, new_listeners) = state.accept(report()).unwrap();
    assert_eq!(new_frame.len(), 2);
    assert_eq!(new_listeners.len(), 1);
    for current in new_listeners {
        current.receive_frame(&new_frame);
    }
    let frames = listener.frames.lock();
    assert_eq!(frames.len(), 1);
    assert_eq!(frames[0].as_slice(), new_frame.as_slice());
}

#[def_test]
fn oversized_input_frame_reports_loss_at_next_sync() {
    let mut state = EndpointState::new();
    let listener = Arc::new(RecordingListener {
        frames: Mutex::new(Vec::new()),
    });
    let subscriber: Arc<dyn InputEventListener> = listener.clone();
    state.listeners.push(Arc::downgrade(&subscriber));

    for _ in 0..=MAX_FRAME_EVENTS {
        assert!(state.accept(timed(EventType::Key as u16, 30, 1)).is_none());
    }
    let mut sync = report();
    sync.time = SystemTime::from_unix_seconds(1_700_000_000);
    sync.monotonic = MonotonicInstant::from_span_since_origin(ktime_types::TimeSpan::from_secs(45));
    let (frame, _) = state.accept(sync).unwrap();
    assert_eq!(frame.len(), 2);
    for event in &frame {
        assert_eq!(event.time, sync.time);
        assert_eq!(event.monotonic, sync.monotonic);
    }
    assert_eq!(frame[0].event.code, SYN_DROPPED);
    assert_eq!(frame[1].event.code, SYN_REPORT);
    assert!(state.key_state.get(30));
}
