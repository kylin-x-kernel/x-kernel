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
    general::{__kernel_old_time_t, __kernel_suseconds_t},
    ioctl::{EVIOCGID, EVIOCGRAB, EVIOCGVERSION},
};
use posix_types::{InputId, UserPtr};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::dynamic::DynamicDeviceDir;

const MAX_CLIENT_EVENTS: usize = 256;
const SYN_REPORT: u16 = 0;
const SYN_DROPPED: u16 = 3;

struct ClientState {
    events: VecDeque<TimedInputEvent>,
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
                is_detached: false,
            }),
            poll_set: PollSet::new(),
        }
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
        let mut read = 0;
        for out in buf.chunks_exact_mut(size_of::<InputEvent>()) {
            let Some(TimedInputEvent { time, event }) = state.events.pop_front() else {
                break;
            };
            let input_event = InputEvent {
                time: KernelTimeval {
                    tv_sec: time.unix_seconds() as _,
                    tv_usec: (time.subsec_nanos() / 1_000) as _,
                },
                event_type: event.event_type,
                code: event.code,
                value: event.value as _,
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
            let time = frame[frame.len() - 1].time;
            for code in [SYN_DROPPED, SYN_REPORT] {
                state.events.push_back(TimedInputEvent {
                    time,
                    event: Event::new(EventType::Synchronization as u16, code, 0),
                });
            }
        } else {
            state.events.extend(frame.iter().copied());
        }
        drop(state);
        self.poll_set.wake();
    }

    fn detach(&self) {
        let mut state = self.state.lock();
        state.is_detached = true;
        state.events.clear();
        drop(state);
        self.poll_set.wake();
    }
}

impl Pollable for EventClient {
    fn poll(&self) -> IoEvents {
        let state = self.state.lock();
        if state.is_detached {
            IoEvents::ERR | IoEvents::HUP
        } else if state.events.is_empty() {
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

pub struct EventDev {
    device: ClassDevice<InputDeviceImpl>,
    endpoint: Arc<InputEndpoint>,
    ev_bits: Bitmap<{ EventType::COUNT as usize }>,
}

impl EventDev {
    pub fn new(device: ClassDevice<InputDeviceImpl>, endpoint: Arc<InputEndpoint>) -> Self {
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
pub struct KernelTimeval {
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
pub extern "C" fn ongkey() {
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

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
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
                        #[allow(clippy::single_match)]
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

#[cfg(unittest)]
mod tests {
    use ktime_types::SystemTime;
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
        let time = SystemTime::UNIX_EPOCH;
        [
            TimedInputEvent {
                time,
                event: Event::new(EventType::Key as u16, 30, 1),
            },
            TimedInputEvent {
                time,
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
        assert_eq!(slow_state.events[0].event.code, SYN_DROPPED);
        assert_eq!(slow_state.events[1].event.code, SYN_REPORT);
        drop(slow_state);
        assert_eq!(fast.state.lock().events.len(), input_frame.len());
        assert!(fast.poll().contains(IoEvents::IN));
    }
}
