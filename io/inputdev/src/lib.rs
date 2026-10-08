// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Input event collection and distribution to independent consumers.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

use alloc::{
    boxed::Box,
    sync::{Arc, Weak},
    vec::Vec,
};

use bitmaps::Bitmap;
use kclass::{
    ClassDevice, Event, EventType, InputDevice, InputDeviceImpl, InputEventNotifier,
    input_devices as class_input_devices, subscribe_input_available,
};
use kdevice::{DeviceId, subscribe_device_removed};
use ksync::Mutex;
use ktime_types::SystemTime;
use kworkng::{SubmitError, WorkHandle};
use lazyinit::LazyInit;

const KEY_COUNT: usize = EventType::Key.bits_count();
const MAX_FRAME_EVENTS: usize = 256;
const POLL_EVENTS_PER_ROUND: usize = 64;
const POLL_ROUNDS_PER_WORK: usize = 2;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

type InputPoller = WorkHandle<Box<dyn Fn() + Send + Sync>>;
type FrameDelivery = (Vec<TimedInputEvent>, Vec<Arc<dyn InputEventListener>>);

/// An input event timestamped when the input layer receives it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimedInputEvent {
    /// Event arrival time.
    pub time: SystemTime,
    /// Driver-provided event.
    pub event: Event,
}

/// An independent consumer of complete input event frames.
///
/// Callbacks run in task context, outside the input endpoint lock. A listener
/// must bound its own buffering so a slow consumer cannot hold the device queue.
pub trait InputEventListener: Send + Sync {
    /// Receive one frame ending in `EV_SYN/SYN_REPORT`.
    fn receive_frame(&self, frame: &[TimedInputEvent]);

    /// Notify the consumer that its device has been removed.
    fn detach(&self);
}

struct EndpointState {
    listeners: Vec<Weak<dyn InputEventListener>>,
    pending_listeners: Vec<Weak<dyn InputEventListener>>,
    frame: Vec<TimedInputEvent>,
    key_state: Bitmap<KEY_COUNT>,
    is_discarding_frame: bool,
    is_detached: bool,
}

impl EndpointState {
    fn new() -> Self {
        Self {
            listeners: Vec::new(),
            pending_listeners: Vec::new(),
            frame: Vec::with_capacity(MAX_FRAME_EVENTS),
            key_state: Bitmap::new(),
            is_discarding_frame: false,
            is_detached: false,
        }
    }

    fn add_listener(&mut self, listener: &Arc<dyn InputEventListener>) {
        if self.frame.is_empty() && !self.is_discarding_frame {
            self.listeners.push(Arc::downgrade(listener));
        } else {
            // A new listener must not receive a frame that started earlier.
            self.pending_listeners.push(Arc::downgrade(listener));
        }
    }

    fn accept(&mut self, timed: TimedInputEvent) -> Option<FrameDelivery> {
        let event = timed.event;
        if event.event_type == EventType::Key as u16
            && (event.code as usize) < KEY_COUNT
            && matches!(event.value, 0 | 1)
        {
            self.key_state.set(event.code as usize, event.value == 1);
        }

        let is_report =
            event.event_type == EventType::Synchronization as u16 && event.code == SYN_REPORT;
        if self.is_discarding_frame {
            if !is_report {
                return None;
            }
            self.is_discarding_frame = false;
            self.frame.push(TimedInputEvent {
                time: timed.time,
                event: Event::new(EventType::Synchronization as u16, SYN_DROPPED, 0),
            });
        } else if self.frame.len() == MAX_FRAME_EVENTS {
            // A malformed device must not grow the pending frame indefinitely.
            self.frame.clear();
            self.is_discarding_frame = true;
            if !is_report {
                return None;
            }
            self.is_discarding_frame = false;
            self.frame.push(TimedInputEvent {
                time: timed.time,
                event: Event::new(EventType::Synchronization as u16, SYN_DROPPED, 0),
            });
        }
        self.frame.push(timed);
        if !is_report {
            return None;
        }

        let listeners: Vec<_> = self.listeners.iter().filter_map(Weak::upgrade).collect();
        self.listeners
            .retain(|listener| listener.strong_count() != 0);
        let frame = if listeners.is_empty() {
            Vec::new()
        } else {
            self.frame.clone()
        };
        self.frame.clear();
        self.listeners.append(&mut self.pending_listeners);
        Some((frame, listeners))
    }
}

struct EndpointNotifier {
    endpoint: Weak<InputEndpoint>,
}

impl InputEventNotifier for EndpointNotifier {
    fn notify_events(&self) {
        if let Some(endpoint) = self.endpoint.upgrade() {
            let _ = endpoint.schedule_poll();
        }
    }
}

/// Task-context input source for one class device.
///
/// This endpoint drains the driver even when no user has opened its device
/// file, allowing the hardware queue to keep making progress.
pub struct InputEndpoint {
    device: ClassDevice<InputDeviceImpl>,
    poller: LazyInit<InputPoller>,
    state: Mutex<EndpointState>,
}

impl InputEndpoint {
    fn new(
        device: ClassDevice<InputDeviceImpl>,
    ) -> Result<Arc<Self>, kclass::prelude::DriverError> {
        let endpoint = Arc::new(Self {
            device,
            poller: LazyInit::new(),
            state: Mutex::new(EndpointState::new()),
        });
        let weak_endpoint = Arc::downgrade(&endpoint);
        let poller = WorkHandle::new(Box::new(move || {
            if let Some(endpoint) = weak_endpoint.upgrade() {
                endpoint.poll_background();
            }
        }) as Box<dyn Fn() + Send + Sync>)
        .map_err(|_| kclass::prelude::DriverError::BadState)?;
        endpoint.poller.init_once(poller);
        Ok(endpoint)
    }

    fn schedule_poll(&self) -> Result<(), SubmitError> {
        // A running callback can accept a successor. AlreadyQueued means that
        // an unclaimed successor will observe this notification instead.
        match self.poller.queue_default(None) {
            Ok(_) | Err(SubmitError::AlreadyQueued) => Ok(()),
            Err(error) => Err(error),
        }
    }

    fn start(self: &Arc<Self>) -> Result<(), kclass::prelude::DriverError> {
        let notifier: Arc<dyn InputEventNotifier> = Arc::new(EndpointNotifier {
            endpoint: Arc::downgrade(self),
        });
        if let Err(error) = self.device.set_event_notifier(Some(notifier)) {
            let _ = self.poller.shutdown();
            return Err(error);
        }
        // The device may have completed events before the input layer attached.
        if self.schedule_poll().is_err() {
            let _ = self.device.set_event_notifier(None);
            let _ = self.poller.shutdown();
            return Err(kclass::prelude::DriverError::BadState);
        }
        Ok(())
    }

    fn poll_background(&self) {
        for _ in 0..POLL_ROUNDS_PER_WORK {
            if !self.poll_once(POLL_EVENTS_PER_ROUND) {
                return;
            }
        }
        let is_detached = self.state.lock().is_detached;
        if !is_detached {
            let _ = self.schedule_poll();
        }
    }

    fn poll_once(&self, budget: usize) -> bool {
        if self.state.lock().is_detached {
            return false;
        }
        for _ in 0..budget {
            if !self.device.is_available() {
                return false;
            }
            match self.device.read_event() {
                Ok(event) => self.accept_event(TimedInputEvent {
                    time: ktime::realtime(),
                    event,
                }),
                Err(kclass::prelude::DriverError::WouldBlock) => {
                    return false;
                }
                Err(error) => {
                    warn!(
                        "input: failed to read event from {}: {error:?}",
                        self.device.name()
                    );
                    return false;
                }
            }
        }
        true
    }

    fn accept_event(&self, event: TimedInputEvent) {
        let ready = {
            let mut state = self.state.lock();
            if state.is_detached {
                return;
            }
            state.accept(event)
        };
        if let Some((frame, listeners)) = ready {
            for listener in listeners {
                listener.receive_frame(&frame);
            }
        }
    }

    /// Register one open device file for independent event delivery.
    ///
    /// # Errors
    ///
    /// Returns `BadState` after the device has been detached.
    pub fn subscribe(
        &self,
        listener: Arc<dyn InputEventListener>,
    ) -> Result<(), kclass::prelude::DriverError> {
        let mut state = self.state.lock();
        if state.is_detached || !self.device.is_available() {
            return Err(kclass::prelude::DriverError::BadState);
        }
        state.add_listener(&listener);
        Ok(())
    }

    /// Stop delivery to an open device file.
    pub fn unsubscribe(&self, listener: &Arc<dyn InputEventListener>) {
        let mut state = self.state.lock();
        state.listeners.retain(|weak| {
            weak.upgrade()
                .is_some_and(|existing| !Arc::ptr_eq(&existing, listener))
        });
        state.pending_listeners.retain(|weak| {
            weak.upgrade()
                .is_some_and(|existing| !Arc::ptr_eq(&existing, listener))
        });
    }

    /// Snapshot the device-wide pressed-key state for `EVIOCGKEY`.
    pub fn key_state(&self) -> Vec<u8> {
        self.state.lock().key_state.as_bytes().to_vec()
    }

    /// Stop the worker and notify all consumers during device removal.
    pub fn detach(&self) {
        let listeners = {
            let mut state = self.state.lock();
            if state.is_detached {
                return;
            }
            state.is_detached = true;
            let listeners = state
                .listeners
                .iter()
                .chain(state.pending_listeners.iter())
                .filter_map(Weak::upgrade)
                .collect::<Vec<_>>();
            state.listeners.clear();
            state.pending_listeners.clear();
            state.frame.clear();
            listeners
        };
        let _ = self.device.set_event_notifier(None);
        if let Err(error) = self.poller.shutdown() {
            warn!(
                "input: failed to stop event worker for {}: {error:?}",
                self.device.name()
            );
        }
        for listener in listeners {
            listener.detach();
        }
    }
}

static INPUT_ENDPOINTS: LazyInit<Mutex<Vec<Arc<InputEndpoint>>>> = LazyInit::new();

/// Initialize input devices and subscribe to later class activation/removal.
pub fn init_input() {
    info!("Initialize input subsystem...");
    INPUT_ENDPOINTS.init_once(Mutex::new(Vec::new()));
    subscribe_device_removed(Arc::new(remove_input_device));
    subscribe_input_available(Arc::new(register_input_device));
    for device in class_input_devices() {
        register_input_device(device);
    }
}

fn register_input_device(device: ClassDevice<InputDeviceImpl>) {
    match input_endpoint(device.clone()) {
        Ok(_) => info!("  input device activated: {}", device.name()),
        Err(error) => warn!("input: failed to activate {}: {error:?}", device.name()),
    }
}

fn remove_input_device(id: DeviceId) {
    let endpoint = {
        let mut endpoints = INPUT_ENDPOINTS.lock();
        endpoints
            .iter()
            .position(|endpoint| endpoint.device.id() == id)
            .map(|index| endpoints.swap_remove(index))
    };
    if let Some(endpoint) = endpoint {
        endpoint.detach();
    }
}

/// Return the unique event endpoint for an active input class device.
///
/// Device-node publication can precede the class availability callback, so
/// devfs also uses this function to create the endpoint on first sight.
///
/// # Errors
///
/// Returns `BadState` for removed devices or unavailable worker queues, or
/// propagates a driver notification-registration failure.
pub fn input_endpoint(
    device: ClassDevice<InputDeviceImpl>,
) -> Result<Arc<InputEndpoint>, kclass::prelude::DriverError> {
    let mut endpoints = INPUT_ENDPOINTS.lock();
    if let Some(endpoint) = endpoints
        .iter()
        .find(|endpoint| endpoint.device.id() == device.id())
    {
        return Ok(endpoint.clone());
    }
    if !device.is_available() {
        return Err(kclass::prelude::DriverError::BadState);
    }
    let endpoint = InputEndpoint::new(device.clone())?;
    endpoint.start()?;
    if !device.is_available() {
        endpoint.detach();
        return Err(kclass::prelude::DriverError::BadState);
    }
    endpoints.push(endpoint.clone());
    Ok(endpoint)
}

/// Return a snapshot of active input device handles.
pub fn input_devices() -> Vec<ClassDevice<InputDeviceImpl>> {
    INPUT_ENDPOINTS
        .lock()
        .iter()
        .map(|endpoint| endpoint.device.clone())
        .collect()
}

#[cfg(unittest)]
mod tests {
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
        let (frame, _) = state.accept(report()).unwrap();
        assert_eq!(frame.len(), 2);
        assert_eq!(frame[0].event.code, SYN_DROPPED);
        assert_eq!(frame[1].event.code, SYN_REPORT);
        assert!(state.key_state.get(30));
    }
}
