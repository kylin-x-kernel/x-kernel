// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::VecDeque, string::ToString, sync::Arc, vec, vec::Vec};

use bitmaps::Bitmap;
use inputdev::{InputEndpoint, InputEventListener, TimedInputEvent};
use kclass::{
    ClassDevice,
    prelude::{Event, EventType, InputDevice, InputDeviceImpl},
};
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use kvfs::{
    DeviceFileOps, DirMapping, NodeFlags, SimpleDir, SimpleFs, VfsFile, VfsFileBuilder, VfsInode,
    VfsResult,
};
use linux_raw_sys::{
    general::{
        __kernel_old_time_t, __kernel_old_timeval, __kernel_suseconds_t, CLOCK_BOOTTIME,
        CLOCK_MONOTONIC, CLOCK_REALTIME,
    },
    ioctl::{EVIOCGID, EVIOCGRAB, EVIOCGVERSION, EVIOCSCLOCKID},
};
use posix_types::{InputId, SystemTimeLike, TimeSpanLike, UserConstPtr, UserPtr};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::dynamic::DynamicDeviceDir;

const MAX_CLIENT_EVENTS: usize = 256;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum EventClock {
    Realtime,
    Monotonic,
    Boottime,
}

impl EventClock {
    fn from_clock_id(clock_id: i32) -> KResult<Self> {
        // Linux evdev_set_clk_type accepts only these three clocks, even when
        // clock_gettime supports additional clock ids (Linux v6.12, evdev.c).
        match clock_id as u32 {
            CLOCK_REALTIME => Ok(Self::Realtime),
            CLOCK_MONOTONIC => Ok(Self::Monotonic),
            CLOCK_BOOTTIME => Ok(Self::Boottime),
            _ => Err(KError::InvalidInput),
        }
    }

    fn input_event(self, timed: TimedInputEvent) -> InputEvent {
        let time = match self {
            Self::Realtime => __kernel_old_timeval::from_system_time(timed.time),
            // Match clock_gettime(CLOCK_BOOTTIME): suspend time is not yet
            // accounted separately. Keep the selections distinct for flushes.
            Self::Monotonic | Self::Boottime => {
                __kernel_old_timeval::from_time_span(timed.monotonic.span_since_origin())
            }
        };
        InputEvent {
            time: KernelTimeval {
                tv_sec: time.tv_sec,
                tv_usec: time.tv_usec,
            },
            event_type: timed.event.event_type,
            code: timed.event.code,
            value: timed.event.value as _,
        }
    }
}

struct ClientState {
    events: VecDeque<InputEvent>,
    clock: EventClock,
    // InputEndpoint supplies complete frames. Only a clock-change drop marker
    // can be queued without a following report; it must not be readable yet.
    is_waiting_for_report: bool,
    is_detached: bool,
}

struct EventClient {
    state: Mutex<ClientState>,
    poll_set: PollSet,
}

impl EventClient {
    fn new() -> Self {
        Self {
            state: Mutex::new(ClientState {
                events: VecDeque::with_capacity(MAX_CLIENT_EVENTS),
                clock: EventClock::Realtime,
                is_waiting_for_report: false,
                is_detached: false,
            }),
            poll_set: PollSet::new(),
        }
    }

    fn set_clock_id(&self, clock_id: i32) -> KResult<()> {
        let mut state = self.state.lock();
        if state.is_detached {
            return Err(KError::NoSuchDevice);
        }
        let clock = EventClock::from_clock_id(clock_id)?;
        if state.clock == clock {
            return Ok(());
        }
        state.clock = clock;
        if !state.events.is_empty() {
            state.events.clear();
            let (time, monotonic) = ktime::realtime_and_monotonic();
            state.events.push_back(clock.input_event(TimedInputEvent {
                time,
                monotonic,
                event: Event::new(EventType::Synchronization as u16, SYN_DROPPED, 0),
            }));
            state.is_waiting_for_report = true;
        }
        // Like Linux, the drop marker becomes readable only when a subsequent
        // SYN_REPORT arrives. Changing the clock alone must not wake readers.
        Ok(())
    }

    fn read(&self, buf: &mut [u8]) -> KResult<usize> {
        let mut state = self.state.lock();
        if state.is_detached {
            return Err(KError::NoSuchDevice);
        }
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() < size_of::<InputEvent>() {
            return Err(KError::InvalidInput);
        }
        if state.is_waiting_for_report {
            return Err(KError::WouldBlock);
        }
        let mut read = 0;
        for out in buf.chunks_exact_mut(size_of::<InputEvent>()) {
            let Some(input_event) = state.events.pop_front() else {
                break;
            };
            out.copy_from_slice(input_event.as_bytes());
            read += out.len();
        }
        if read == 0 {
            Err(KError::WouldBlock)
        } else {
            Ok(read)
        }
    }
}

impl InputEventListener for EventClient {
    fn receive_frame(&self, frame: &[TimedInputEvent]) {
        if frame.is_empty() {
            return;
        }
        let mut state = self.state.lock();
        if state.is_detached {
            return;
        }
        if state.events.len() + frame.len() > MAX_CLIENT_EVENTS {
            state.events.clear();
            let clock = state.clock;
            let last = frame[frame.len() - 1];
            for code in [SYN_DROPPED, SYN_REPORT] {
                state.events.push_back(clock.input_event(TimedInputEvent {
                    event: Event::new(EventType::Synchronization as u16, code, 0),
                    ..last
                }));
            }
        } else {
            let clock = state.clock;
            state
                .events
                .extend(frame.iter().map(|timed| clock.input_event(*timed)));
        }
        state.is_waiting_for_report = false;
        drop(state);
        self.poll_set.wake();
    }

    fn detach(&self) {
        let mut state = self.state.lock();
        state.is_detached = true;
        state.events.clear();
        state.is_waiting_for_report = false;
        drop(state);
        self.poll_set.wake();
    }
}

impl Pollable for EventClient {
    fn poll(&self) -> IoEvents {
        let state = self.state.lock();
        if state.is_detached {
            IoEvents::ERR | IoEvents::HUP
        } else if state.events.is_empty() || state.is_waiting_for_report {
            IoEvents::empty()
        } else {
            IoEvents::IN
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if !events.is_empty() {
            context.register(&self.poll_set)?;
        }
        Ok(())
    }
}

pub(crate) struct EventDev {
    device: ClassDevice<InputDeviceImpl>,
    endpoint: Arc<InputEndpoint>,
    ev_bits: Bitmap<{ EventType::COUNT as usize }>,
}

impl EventDev {
    pub(crate) fn new(device: ClassDevice<InputDeviceImpl>, endpoint: Arc<InputEndpoint>) -> Self {
        let mut ev_bits = Bitmap::new();
        for i in 0..EventType::COUNT {
            let Some(ty) = EventType::from_repr(i) else {
                continue;
            };
            if device
                .get_event_bits(ty, &mut [])
                .is_ok_and(|success| success)
            {
                ev_bits.set(i as usize, true);
            }
        }

        Self {
            device,
            endpoint,
            ev_bits,
        }
    }

    pub(crate) fn detach(&self) {
        self.endpoint.detach();
    }

    fn get_event_bits(&self, arg: usize, size: usize, ty: u8) -> KResult<usize> {
        let user_bits = UserPtr::<u8>::from(arg);
        if ty == 0 {
            let len = self.ev_bits.as_bytes().len().min(size);
            user_bits.write_vm_slice(&self.ev_bits.as_bytes()[..len])?;
            Ok(len)
        } else {
            let ty = EventType::from_repr(ty).ok_or(KError::InvalidInput)?;
            let mut bits = vec![0u8; size];
            match self.device.get_event_bits(ty, &mut bits) {
                Ok(true) => {}
                Ok(false) => {
                    debug!("No events for {ty:?}");
                }
                Err(err) => {
                    warn!("Failed to get event bits: {err:?}");
                }
            }
            let len = bits.len().min(ty.bits_count().div_ceil(8));
            user_bits.write_vm_slice(&bits[..len])?;
            Ok(len)
        }
    }
}

fn return_str(arg: usize, size: usize, s: &str) -> KResult<usize> {
    if size == 0 {
        return Ok(0);
    }
    let value = evdev_string_bytes(s, size);
    let len = value.len();
    UserPtr::<u8>::from(arg).write_vm_slice(&value)?;
    Ok(len)
}

fn evdev_string_bytes(s: &str, size: usize) -> Vec<u8> {
    let len = s.len().saturating_add(1).min(size);
    let content_len = s.len().min(len);
    let mut value = vec![0; len];
    value[..content_len].copy_from_slice(&s.as_bytes()[..content_len]);
    value
}

fn return_zero_bits(arg: usize, size: usize, bits: usize) -> KResult<usize> {
    let len = bits.div_ceil(8).min(size);
    let zeros = vec![0u8; len];
    UserPtr::<u8>::from(arg).write_vm_slice(&zeros)?;
    Ok(len)
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable)]
pub(crate) struct KernelTimeval {
    pub tv_sec: __kernel_old_time_t,
    pub tv_usec: __kernel_suseconds_t,
}

#[repr(C)]
#[derive(FromBytes, IntoBytes, Immutable)]
struct InputEvent {
    time: KernelTimeval,
    event_type: u16,
    code: u16,
    value: i32,
}

#[unsafe(no_mangle)]
#[inline(never)]
pub(crate) extern "C" fn ongkey() {
    core::hint::black_box(());
}

impl DeviceFileOps for EventDev {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let client = Arc::new(EventClient::new());
        self.endpoint
            .subscribe(client.clone())
            .map_err(|_| KError::NoSuchDevice)?;
        file.set_private_data(client);
        file.stream_open();
        Ok(())
    }

    fn release(&self, _inode: &VfsInode, file: &VfsFile) -> VfsResult<()> {
        if let Some(client) = file.private_data_get::<EventClient>() {
            let listener: Arc<dyn InputEventListener> = client;
            self.endpoint.unsubscribe(&listener);
        }
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        if !self.device.is_available() {
            return Err(KError::NoSuchDevice);
        }
        let client = file
            .private_data_get::<EventClient>()
            .ok_or(KError::BadFileDescriptor)?;
        client.read(buf)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn ioctl(&self, file: &VfsFile, _cred: &kcred::Cred, cmd: u32, arg: usize) -> VfsResult<usize> {
        if !self.device.is_available() {
            return Err(KError::NoSuchDevice);
        }
        match cmd {
            EVIOCGVERSION => {
                UserPtr::<u32>::from(arg).write_vm(0x10001)?;
                Ok(0)
            }
            EVIOCGID => {
                let device_id = self.device.device_id();
                UserPtr::<InputId>::from(arg).write_vm(InputId {
                    bus_type: device_id.bus_type,
                    vendor: device_id.vendor,
                    product: device_id.product,
                    version: device_id.version,
                })?;
                Ok(0)
            }
            EVIOCSCLOCKID => {
                let clock_id = UserConstPtr::<i32>::from(arg).read_vm()?;
                let client = file
                    .private_data_get::<EventClient>()
                    .ok_or(KError::BadFileDescriptor)?;
                client.set_clock_id(clock_id)?;
                Ok(0)
            }
            EVIOCGRAB => Ok(0),
            other => {
                // variable-length command
                let mut tmp = other;
                let nr = (tmp & 0xff) as u8;
                tmp >>= 8;
                let ty = (tmp & 0xff) as u8;
                tmp >>= 8;
                let size = (tmp & 0x3fff) as usize;
                tmp >>= 14;
                let dir = tmp & 0x3;

                if ty != b'E' {
                    warn!("unknown ioctl for evdev: {cmd} {arg}");
                    return Err(KError::InvalidInput);
                }

                match dir {
                    // IOC_WRITE
                    1 => return Err(KError::InvalidInput),
                    // IOC_READ
                    2 => {
                        match nr {
                            // EVIOCGNAME
                            0x06 => {
                                return return_str(arg, size, self.device.name());
                            }
                            // EVIOCGPHYS
                            0x07 => {
                                let value = self.device.physical_location().to_string();
                                return return_str(arg, size, &value);
                            }
                            // EVIOCGUNIQ
                            0x08 => {
                                let value = self.device.unique_id().to_string();
                                return return_str(arg, size, &value);
                            }
                            // EVIOCGPROP
                            0x09 => {
                                // For some reasons virtio does not provide prop
                                // bits for now
                                return Ok(0);
                            }
                            // EVIOCGKEY
                            0x18 => {
                                let key_bits = self.endpoint.key_state();
                                let len = key_bits.len().min(size);
                                UserPtr::<u8>::from(arg).write_vm_slice(&key_bits[..len])?;
                                return Ok(len);
                            }
                            // EVIOCGLED
                            0x19 => {
                                return return_zero_bits(arg, size, EventType::Led.bits_count());
                            }
                            // EVIOCGSND
                            0x1a => {
                                return return_zero_bits(arg, size, EventType::Sound.bits_count());
                            }
                            // EVIOCGSW
                            0x1b => {
                                return return_zero_bits(arg, size, EventType::Switch.bits_count());
                            }
                            _ => {}
                        }
                        if nr & !EventType::MAX == EventType::COUNT {
                            return self.get_event_bits(arg, size, nr & EventType::MAX);
                        }
                        const ABS_CNT: u8 = 0x40;
                        if nr & !(ABS_CNT - 1) == ABS_CNT {
                            // TODO: abs info
                            return Ok(0);
                        }
                        return Err(KError::InvalidInput);
                    }
                    _ => {}
                }

                Err(KError::InvalidInput)
            }
        }
    }

    fn poll(&self, file: &VfsFile) -> IoEvents {
        if !self.device.is_available() {
            return IoEvents::ERR | IoEvents::HUP;
        }
        file.private_data_get::<EventClient>()
            .map_or(IoEvents::ERR | IoEvents::HUP, |client| client.poll())
    }

    fn register_poll(
        &self,
        file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        let client = file
            .private_data_get::<EventClient>()
            .ok_or(PollRegisterError::InvalidState)?;
        client.register(context, events)
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "input",
        SimpleDir::new_maker(fs, Arc::new(DynamicDeviceDir::input())),
    );
}

#[unittest::mod_test]
mod tests {
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
        let monotonic =
            MonotonicInstant::from_span_since_origin(TimeSpan::from_nanos(45_987_654_321));
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
}
