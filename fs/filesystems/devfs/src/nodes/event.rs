// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{collections::VecDeque, string::ToString, sync::Arc, vec, vec::Vec};
use core::sync::atomic::{AtomicBool, Ordering};

use bitmaps::Bitmap;
use inputdev::{InputEndpoint, InputEventListener, TimedInputEvent};
use kclass::{
    ABS_CNT, AbsInfo, ClassDevice, PROP_BITMAP_LEN,
    prelude::{Event, EventType, InputDevice, InputDeviceImpl},
};
use kerrno::{KError, KResult};
use khal::time::monotonic_time;
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use ktask::{Expiration, Timer, TimerControl, TimerHandler, future::block_on};
use ktime_types::TimeSpan;
use kvfs::{
    DeviceFileOps, DirMapping, NodeFlags, SimpleDir, SimpleFs, VfsFile, VfsFileBuilder, VfsInode,
    VfsResult, poll_file_io,
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
    repoll_timer: Option<Timer<InputRepoll>>,
}

struct InputRepoll {
    poll_set: PollSet,
    device: ClassDevice<InputDeviceImpl>,
    is_armed: AtomicBool,
}

impl TimerHandler for InputRepoll {
    fn on_expiration(&self, timer: TimerControl<'_>, _: Expiration) {
        // Publish idle before inspecting waiters. A racing registration can
        // start a successor; the CAS below prevents this callback replacing it.
        self.is_armed.store(false, Ordering::Release);
        if !self.poll_set.has_waiters() {
            return;
        }
        if self.device.is_available() && self.device.poll_event_ready() == Some(false) {
            if self
                .is_armed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let deadline = monotonic_time() + TimeSpan::from_millis(10);
                if timer.rearm(deadline).is_err() {
                    self.is_armed.store(false, Ordering::Release);
                    self.poll_set.wake();
                }
            }
            return;
        }
        // Ready, detached, or a legacy driver: let the task do its normal poll.
        self.poll_set.wake();
    }
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
            repoll_timer: None,
        }
    }

    fn new_with_device(device: ClassDevice<InputDeviceImpl>) -> Self {
        let mut client = Self::new();
        let repoll_timer = Timer::new(InputRepoll {
            poll_set: client.poll_set.clone(),
            device,
            is_armed: AtomicBool::new(false),
        });
        client.repoll_timer = Some(repoll_timer);
        client
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
        if let Some(timer) = &self.repoll_timer {
            let handler = timer.handler();
            if handler
                .is_armed
                .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
                .is_ok()
            {
                let deadline = monotonic_time() + TimeSpan::from_millis(10);
                // SAFETY: VFS poll registration runs after scheduler/timer
                // initialization. The owned timer drains delivery on drop; its
                // handler only uses the IRQ-safe readiness hint and poll set.
                if unsafe { timer.start(deadline) }.is_err() {
                    handler.is_armed.store(false, Ordering::Release);
                    return Err(PollRegisterError::InvalidState);
                }
            }
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

    /// Handles `EVIOCGPROP`, the `INPUT_PROP_*` capability bitmap.
    ///
    /// The reply is zero-padded to the full `usize`-rounded bitmap length, but
    /// the returned length is clipped to the caller's buffer like Linux
    /// `bits_to_user`.
    fn get_prop_bits(&self, arg: usize, size: usize) -> KResult<usize> {
        if size == 0 {
            return Ok(0);
        }
        let mut bits = [0u8; PROP_BITMAP_LEN];
        if let Err(err) = self.device.get_prop_bits(&mut bits) {
            warn!("Failed to get prop bits: {err:?}");
            return Err(KError::Io);
        }
        let len = bits.len().min(size);
        UserPtr::<u8>::from(arg).write_vm_slice(&bits[..len])?;
        Ok(len)
    }

    /// Handles `EVIOCGABS(code)` for `code` in `0..ABS_CNT`.
    ///
    /// Mirrors Linux `evdev_do_ioctl`: a device without `EV_ABS` capability
    /// returns `EINVAL`, while an axis the device does not report succeeds with
    /// the zero-filled `input_absinfo` that the input core would contain. The
    /// live scalar value comes from inputdev, which observes `EV_ABS` events
    /// even when no evdev file is open; MT payload axes stay outside the
    /// scalar snapshot.
    fn get_abs_info(&self, arg: usize, size: usize, code: u8) -> KResult<usize> {
        // Check support before honoring a zero-length request: Linux rejects
        // `EVIOCGABS` on devices without `EV_ABS` even if size is zero.
        if !self.ev_bits.get(EventType::Absolute as usize) {
            return Err(KError::InvalidInput);
        }
        if size == 0 {
            return Ok(0);
        }
        let info = match self.device.abs_info(u16::from(code)) {
            Ok(info) => info,
            Err(err) => {
                warn!("Failed to query abs info for axis {code}: {err:?}");
                return Err(KError::Io);
            }
        };
        let value = self
            .endpoint
            .abs_value(u16::from(code))
            .ok_or(KError::InvalidInput)?;
        let abs = abs_info_reply(info, value);
        let bytes = abs.as_bytes();
        let len = bytes.len().min(size);
        UserPtr::<u8>::from(arg).write_vm_slice(&bytes[..len])?;
        // Linux returns 0 for EVIOCGABS; the command encodes the copied size.
        Ok(0)
    }

    fn get_event_bits(&self, arg: usize, size: usize, ty: u8) -> KResult<usize> {
        let user_bits = UserPtr::<u8>::from(arg);
        if ty == 0 {
            let len = self.ev_bits.as_bytes().len().min(size);
            user_bits.write_vm_slice(&self.ev_bits.as_bytes()[..len])?;
            Ok(len)
        } else {
            let ty = EventType::from_repr(ty).ok_or(KError::InvalidInput)?;
            // Linux clips the bitmap to the event type's declared width before
            // copying. Doing the same here bounds the kernel allocation by the
            // ABI size instead of by a user-controlled ioctl length.
            let len = size.min(ty.bits_count().div_ceil(8));
            let mut bits = vec![0u8; len];
            match self.device.get_event_bits(ty, &mut bits) {
                Ok(true) => {}
                Ok(false) => {
                    debug!("No events for {ty:?}");
                }
                Err(err) => {
                    warn!("Failed to get event bits: {err:?}");
                }
            }
            user_bits.write_vm_slice(&bits)?;
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

/// Decodes the axis code from an `EVIOCGABS(code)` request number.
///
/// Linux encodes the axis directly in the low command byte starting at
/// `ABS_CNT`; the generic `EVIOCGBIT` range ends immediately below it.
fn decode_abs_request(nr: u8) -> Option<u8> {
    let code = nr.checked_sub(ABS_CNT as u8)?;
    (usize::from(code) < ABS_CNT).then_some(code)
}

/// Builds the exact reply Linux would send for one `EVIOCGABS` axis.
///
/// An axis the driver does not report is a success with a completely
/// zero-filled descriptor. Linux copies that zeroed slot only for an
/// unadvertised axis, so any core-tracked value is deliberately ignored.
fn abs_info_reply(info: Option<AbsInfo>, value: i32) -> InputAbsInfo {
    info.map_or_else(InputAbsInfo::default, |info| InputAbsInfo::new(info, value))
}

/// Linux `struct input_absinfo`, returned by `EVIOCGABS(code)`.
///
/// `value` is the most recent `EV_ABS` value observed by the input layer.
#[repr(C)]
#[derive(Default, FromBytes, IntoBytes, Immutable)]
struct InputAbsInfo {
    value: i32,
    minimum: i32,
    maximum: i32,
    fuzz: i32,
    flat: i32,
    resolution: i32,
}

impl InputAbsInfo {
    const fn new(info: AbsInfo, value: i32) -> Self {
        Self {
            value,
            minimum: info.min,
            maximum: info.max,
            fuzz: info.fuzz,
            flat: info.flat,
            resolution: info.res,
        }
    }
}

// SAFETY: `ongkey` has one definition in kernel sources. No caller exists in
// this repository, so the QEMU link discards it; if retained, its C ABI must
// remain `fn()` with no return value.
#[unsafe(no_mangle)]
#[inline(never)]
pub(crate) extern "C" fn ongkey() {
    core::hint::black_box(());
}

impl DeviceFileOps for EventDev {
    fn open(&self, _inode: &VfsInode, file: &mut VfsFileBuilder) -> VfsResult<()> {
        let client = Arc::new(EventClient::new_with_device(self.device.clone()));
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
        block_on(poll_file_io(
            client.as_ref(),
            IoEvents::IN,
            file.is_nonblocking(),
            || client.read(buf),
        ))
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
                            0x09 => return self.get_prop_bits(arg, size),
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
                        if let Some(code) = decode_abs_request(nr) {
                            return self.get_abs_info(arg, size, code);
                        }
                        return Err(KError::InvalidInput);
                    }
                    _ => {}
                }

                Err(KError::InvalidInput)
            }
        }
    }

    fn poll(&self, file: &VfsFile) -> Option<IoEvents> {
        if !self.device.is_available() {
            return Some(IoEvents::ERR | IoEvents::HUP);
        }
        Some(
            file.private_data_get::<EventClient>()
                .map_or(IoEvents::ERR | IoEvents::HUP, |client| client.poll()),
        )
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
        SimpleDir::new_maker(fs.clone(), Arc::new(DynamicDeviceDir::input(fs))),
    );
}

unittest::path_mod_test!(tests, "../../ut-tests/event.rs");
