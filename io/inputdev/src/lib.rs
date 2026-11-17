// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Input event collection and distribution to independent consumers.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

use alloc::{
    sync::{Arc, Weak},
    vec::Vec,
};

use bitmaps::Bitmap;
use kalloc_types::KBox;
use kclass::{
    ABS_CNT, ABS_MT_FIRST, ABS_MT_LAST, ABS_MT_SLOT, ClassDevice, Event, EventType, InputDevice,
    InputDeviceImpl, InputEventNotifier, input_devices as class_input_devices,
    subscribe_input_available, subscribe_input_removed,
};
use kdevice::PublicationId;
use ksync::Mutex;
use ktime_types::{MonotonicInstant, SystemTime};
use kworkng::{SubmitError, WorkHandle};
use lazyinit::LazyInit;

const KEY_COUNT: usize = EventType::Key.bits_count();
const MAX_FRAME_EVENTS: usize = 256;
const POLL_EVENTS_PER_ROUND: usize = 64;
const POLL_ROUNDS_PER_WORK: usize = 2;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

type InputPoller = WorkHandle<KBox<dyn Fn() + Send + Sync>>;
type FrameDelivery = (Vec<TimedInputEvent>, Vec<Arc<dyn InputEventListener>>);

/// An input event timestamped when the input layer receives it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct TimedInputEvent {
    /// Realtime sampled when the input layer received this event.
    pub time: SystemTime,
    /// Monotonic timestamp from the same sample, retained across delayed reads
    /// and subsequent wall-clock changes.
    pub monotonic: MonotonicInstant,
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
    /// Linux scalar `input_absinfo.value` snapshots, excluding MT payloads.
    abs_values: [i32; ABS_CNT],
    /// Highest `ABS_MT_SLOT` selector advertised by the device, if any.
    mt_slot_max: Option<i32>,
    /// A slot selector waiting for the first MT payload that flushes it.
    pending_mt_slot: Option<i32>,
    is_discarding_frame: bool,
    is_detached: bool,
}

impl EndpointState {
    fn new(mt_slot_max: Option<i32>) -> Self {
        Self {
            listeners: Vec::new(),
            pending_listeners: Vec::new(),
            frame: Vec::with_capacity(MAX_FRAME_EVENTS),
            key_state: Bitmap::new(),
            abs_values: [0; ABS_CNT],
            mt_slot_max,
            pending_mt_slot: None,
            is_discarding_frame: false,
            is_detached: false,
        }
    }

    /// Records the scalar part of Linux absolute-axis state.
    ///
    /// Linux keeps `ABS_MT_*` payload values in per-slot state, not in
    /// `input_absinfo[code].value`. `ABS_MT_SLOT` is a selector: Linux commits
    /// it to `input_absinfo[ABS_MT_SLOT].value` when the next payload flushes a
    /// new slot. Reproduce that split here so `EVIOCGABS` never exposes one
    /// finger's payload as a device-wide axis position.
    fn observe_abs_value(&mut self, code: u16, value: i32) {
        match code {
            ABS_MT_SLOT => {
                // Linux accepts only selectors below `mt->num_slots`; the
                // advertised maximum is `num_slots - 1`.
                if self
                    .mt_slot_max
                    .is_some_and(|max| (0..=max).contains(&value))
                {
                    self.pending_mt_slot = Some(value);
                }
            }
            ABS_MT_FIRST..=ABS_MT_LAST => {
                if let Some(slot) = self.pending_mt_slot.take() {
                    self.abs_values[usize::from(ABS_MT_SLOT)] = slot;
                }
            }
            _ => {
                if let Some(current) = self.abs_values.get_mut(usize::from(code)) {
                    // The evdev wire value is an unsigned word carrying a
                    // signed Linux `__s32`; preserve the bit pattern.
                    *current = value;
                }
            }
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
        if event.event_type == EventType::Absolute as u16 {
            self.observe_abs_value(event.code, event.value as i32);
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
                monotonic: timed.monotonic,
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
                monotonic: timed.monotonic,
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
        let mt_slot_max = match device.abs_info(ABS_MT_SLOT) {
            Ok(info) => info.map(|info| info.max),
            Err(error) => {
                warn!(
                    "input: failed to query MT slot range for {}: {error:?}",
                    device.name()
                );
                None
            }
        };
        let endpoint = Arc::new(Self {
            device,
            poller: LazyInit::new(),
            state: Mutex::new(EndpointState::new(mt_slot_max)),
        });
        let weak_endpoint = Arc::downgrade(&endpoint);
        let callback: KBox<dyn Fn() + Send + Sync> = KBox::new_nowait(move || {
            if let Some(endpoint) = weak_endpoint.upgrade() {
                endpoint.poll_background();
            }
        })
        .map_err(|_| kclass::prelude::DriverError::NoMemory)?;
        let poller =
            WorkHandle::new(callback).map_err(|_| kclass::prelude::DriverError::BadState)?;
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
                Ok(event) => {
                    let (time, monotonic) = ktime::realtime_and_monotonic();
                    self.accept_event(TimedInputEvent {
                        time,
                        monotonic,
                        event,
                    });
                }
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

    /// Snapshot the value Linux exposes through `EVIOCGABS` for one axis.
    ///
    /// Returns `None` for a code outside the `ABS_CNT` range. Multi-touch
    /// payload axes (`ABS_MT_FIRST..=ABS_MT_LAST`) always read as zero because
    /// Linux keeps their per-slot state outside `input_absinfo`. `ABS_MT_SLOT`
    /// reports the slot last flushed by an MT payload. The default zero is also
    /// the value Linux retains for a registered but not-yet-moved axis.
    pub fn abs_value(&self, code: u16) -> Option<i32> {
        self.state.lock().abs_values.get(usize::from(code)).copied()
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
///
/// Requires sleepable task context as described by [`input_endpoint`].
pub fn init_input() {
    info!("Initialize input subsystem...");
    INPUT_ENDPOINTS.init_once(Mutex::new(Vec::new()));
    subscribe_input_removed(Arc::new(|_, publication| remove_input_device(publication)));
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

fn remove_input_device(publication: PublicationId) {
    let endpoint = {
        let mut endpoints = INPUT_ENDPOINTS.lock();
        endpoints
            .iter()
            .position(|endpoint| endpoint.device.publication_id() == publication)
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
/// # Execution context
/// Requires sleepable task context with IRQs/preemption enabled, outside
/// non-sleepable RCU sections, for worker preparation and device operations.
/// The callback owner uses NOWAIT while the endpoint registry mutex is held,
/// avoiding synchronous reclaim that could re-enter endpoint discovery/removal.
/// This does not make the whole function nonblocking or constrain nested Arc,
/// Vec and worker allocations. Failed callback allocation publishes no endpoint
/// or notifier; its captured Weak is released while the registry is locked.
///
/// # Errors
///
/// Returns `BadState` for removed devices or unavailable worker queues, or
/// propagates a driver notification-registration failure. Returns `NoMemory`
/// if callback owner allocation fails, without replacing an existing endpoint.
pub fn input_endpoint(
    device: ClassDevice<InputDeviceImpl>,
) -> Result<Arc<InputEndpoint>, kclass::prelude::DriverError> {
    let mut endpoints = INPUT_ENDPOINTS.lock();
    if let Some(endpoint) = endpoints
        .iter()
        .find(|endpoint| endpoint.device.publication_id() == device.publication_id())
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

unittest::path_mod_test!(inputdev_unittest, "../ut-tests/inputdev_unittest.rs");
