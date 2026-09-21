// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{string::ToString, sync::Arc, vec, vec::Vec};

use bitmaps::Bitmap;
use kclass::ClassDevice;
#[allow(unused_imports)]
use kclass::prelude::{DriverError, Event, EventType, InputDevice, InputDeviceId, InputDeviceImpl};
use kerrno::{KError, KResult};
use kpoll::{IoEvents, PollContext, PollRegisterError, PollSet, Pollable};
use ksync::Mutex;
use ktime_types::SystemTime;
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

const KEY_CNT: usize = EventType::Key.bits_count();

struct Inner {
    device: Option<ClassDevice<InputDeviceImpl>>,
    read_ahead: Option<(SystemTime, Event)>,
    key_state: Bitmap<KEY_CNT>,
}
impl Inner {
    fn detach(&mut self) {
        self.device = None;
        self.read_ahead = None;
        self.key_state = Bitmap::new();
    }

    fn live_device(&mut self) -> KResult<&mut ClassDevice<InputDeviceImpl>> {
        if self
            .device
            .as_ref()
            .is_none_or(|device| !device.is_available())
        {
            self.detach();
            return Err(KError::NoSuchDevice);
        }

        self.device.as_mut().ok_or(KError::NoSuchDevice)
    }

    fn has_event(&mut self) -> KResult<bool> {
        let needs_read = self.read_ahead.is_none();
        let device = self.live_device()?;
        if needs_read {
            let event_result = device.read_event();
            match event_result {
                Ok(event) => {
                    if event.event_type == EventType::Key as u16 {
                        if event.value == 0 {
                            self.key_state.set(event.code as usize, false);
                        } else if event.value == 1 {
                            self.key_state.set(event.code as usize, true);
                        }
                    }
                    self.read_ahead = Some((ktime::realtime(), event));
                }
                Err(DriverError::WouldBlock) => {}
                Err(err) => {
                    warn!("Failed to read event: {err:?}");
                }
            }
        }
        Ok(self.read_ahead.is_some())
    }
}

pub struct EventDev {
    inner: Mutex<Inner>,
    ev_bits: Bitmap<{ EventType::COUNT as usize }>,
    poll_set: PollSet,
}

impl EventDev {
    pub fn new(device: ClassDevice<InputDeviceImpl>) -> Self {
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
            inner: Mutex::new(Inner {
                device: Some(device),
                read_ahead: None,
                key_state: Bitmap::new(),
            }),
            ev_bits,
            poll_set: PollSet::new(),
        }
    }

    pub(crate) fn detach(&self) {
        let mut inner = self.inner.lock();
        inner.detach();
        drop(inner);
        self.poll_set.wake();
    }

    fn get_event_bits(&self, inner: &mut Inner, arg: usize, size: usize, ty: u8) -> KResult<usize> {
        let user_bits = UserPtr::<u8>::from(arg);
        if ty == 0 {
            let len = self.ev_bits.as_bytes().len().min(size);
            user_bits.write_vm_slice(&self.ev_bits.as_bytes()[..len])?;
            Ok(len)
        } else {
            let ty = EventType::from_repr(ty).ok_or(KError::InvalidInput)?;
            let mut bits = vec![0u8; size];
            let device = inner.live_device()?;
            match device.get_event_bits(ty, &mut bits) {
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
        file.stream_open();
        Ok(())
    }

    fn supports_read(&self) -> bool {
        true
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], _offset: u64) -> VfsResult<usize> {
        let mut inner = self.inner.lock();
        // Linux evdev reports ENODEV before handling a zero-length read once
        // the device has disappeared.
        inner.live_device()?;
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() < size_of::<InputEvent>() {
            return Err(KError::InvalidInput);
        }
        let mut read = 0;
        for out in buf.chunks_exact_mut(size_of::<InputEvent>()) {
            match inner.has_event() {
                Ok(true) => {}
                Ok(false) => break,
                Err(err) if read == 0 => return Err(err),
                // Preserve completed records. The next read observes the
                // terminal detached state and returns ENODEV.
                Err(_) => break,
            }
            let Some((time, event)) = inner.read_ahead.take() else {
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

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        let mut inner = self.inner.lock();
        inner.live_device()?;
        match cmd {
            EVIOCGVERSION => {
                UserPtr::<u32>::from(arg).write_vm(0x10001)?;
                Ok(0)
            }
            EVIOCGID => {
                let device = inner.device.as_ref().ok_or(KError::NoSuchDevice)?;
                let device_id = device.device_id();
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
                                let device = inner.device.as_ref().ok_or(KError::NoSuchDevice)?;
                                return return_str(arg, size, device.name());
                            }
                            // EVIOCGPHYS
                            0x07 => {
                                let device = inner.device.as_ref().ok_or(KError::NoSuchDevice)?;
                                let value = device.physical_location().to_string();
                                return return_str(arg, size, &value);
                            }
                            // EVIOCGUNIQ
                            0x08 => {
                                let device = inner.device.as_ref().ok_or(KError::NoSuchDevice)?;
                                let value = device.unique_id().to_string();
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
                                let key_bits = inner.key_state.as_bytes().to_vec();
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
                            return self.get_event_bits(&mut inner, arg, size, nr & EventType::MAX);
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

    fn poll(&self, _file: &VfsFile) -> IoEvents {
        Pollable::poll(self)
    }

    fn register_poll(
        &self,
        _file: &VfsFile,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        Pollable::register(self, context, events)
    }
}

impl Pollable for EventDev {
    fn poll(&self) -> IoEvents {
        let mut inner = self.inner.lock();
        let mut events = IoEvents::empty();
        match inner.has_event() {
            Ok(has_event) => {
                events.set(IoEvents::IN, has_event);
                events
            }
            Err(_) => IoEvents::ERR | IoEvents::HUP,
        }
    }

    fn register(
        &self,
        context: &mut PollContext<'_>,
        events: IoEvents,
    ) -> Result<(), PollRegisterError> {
        if events.contains(IoEvents::IN) {
            context.register(&self.poll_set)?;
            // Input devices do not expose a callback-based wake source yet,
            // so retain the existing prompt-repoll behavior for this waiter only.
            context.wake_by_ref();
        }
        Ok(())
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
    use unittest::{assert_eq, def_test};

    use super::evdev_string_bytes;

    #[def_test]
    fn evdev_strings_follow_linux_truncation_and_nul_rules() {
        assert_eq!(evdev_string_bytes("key", 0), b"");
        assert_eq!(evdev_string_bytes("key", 2), b"ke");
        assert_eq!(evdev_string_bytes("key", 3), b"key");
        assert_eq!(evdev_string_bytes("key", 4), b"key\0");
        assert_eq!(evdev_string_bytes("key", 8), b"key\0");
    }
}
