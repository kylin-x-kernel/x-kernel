// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::ToString, sync::Arc, vec};
use core::{task::Context, time::Duration};

use bitmaps::Bitmap;
use kclass::ClassDevice;
#[allow(unused_imports)]
use kclass::prelude::{DriverError, Event, EventType, InputDevice, InputDeviceId, InputDeviceImpl};
use kdevice::subscribe_device_removed;
use kerrno::{KError, KResult};
use khal::time::wall_time;
use kpoll::{IoEvents, Pollable};
use ksync::Mutex;
use kvfs::{
    DeviceFileOps, DeviceId, DirMapping, NodeFlags, NodeType, SimpleDir, SimpleFs, VfsFile,
    VfsFileBuilder, VfsInode, VfsResult,
};
use linux_raw_sys::{
    general::{__kernel_old_time_t, __kernel_suseconds_t},
    ioctl::{EVIOCGID, EVIOCGRAB, EVIOCGVERSION},
};
use posix_types::{InputId, UserPtr};
use zerocopy::{FromBytes, Immutable, IntoBytes};

use crate::{DeviceFile, add_device_entry};

const KEY_CNT: usize = EventType::Key.bits_count();

struct Inner {
    device: Option<ClassDevice<InputDeviceImpl>>,
    read_ahead: Option<(Duration, Event)>,
    key_state: Bitmap<KEY_CNT>,
}
impl Inner {
    fn has_event(&mut self) -> bool {
        let Some(device) = self.device.as_mut() else {
            self.read_ahead = None;
            return false;
        };
        if self.read_ahead.is_none() {
            match device.read_event() {
                Ok(event) => {
                    if event.event_type == EventType::Key as u16 {
                        if event.value == 0 {
                            self.key_state.set(event.code as usize, false);
                        } else if event.value == 1 {
                            self.key_state.set(event.code as usize, true);
                        }
                    }
                    self.read_ahead = Some((wall_time(), event));
                }
                Err(DriverError::WouldBlock) => {}
                Err(err) => {
                    warn!("Failed to read event: {err:?}");
                }
            }
        }
        self.read_ahead.is_some()
    }
}

pub struct EventDev {
    inner: Mutex<Inner>,
    ev_bits: Bitmap<{ EventType::COUNT as usize }>,
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
        }
    }

    fn subscribe_removed(self: &Arc<Self>) {
        let weak = Arc::downgrade(self);
        subscribe_device_removed(Arc::new(move |id| {
            let Some(device) = weak.upgrade() else {
                return;
            };
            if device.detach_removed(id) {
                info!("evdev: detached removed input device {:?}", id);
            }
        }));
    }

    fn detach_removed(&self, id: kdevice::DeviceId) -> bool {
        let mut inner = self.inner.lock();
        if inner.device.as_ref().is_none_or(|device| device.id() != id) {
            return false;
        }
        inner.device = None;
        inner.read_ahead = None;
        inner.key_state = Bitmap::new();
        true
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
            let mut inner = self.inner.lock();
            let device = inner.device.as_mut().ok_or(KError::NoSuchDevice)?;
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
    let len = s.len().min(size);
    UserPtr::<u8>::from(arg).write_vm_slice(&s.as_bytes()[..len])?;
    Ok(len)
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
        if buf.is_empty() {
            return Ok(0);
        }
        if buf.len() < size_of::<InputEvent>() {
            return Err(KError::InvalidInput);
        }
        let mut read = 0;
        let mut inner = self.inner.lock();
        for out in buf.chunks_exact_mut(size_of::<InputEvent>()) {
            if !inner.has_event() {
                break;
            }
            let Some((time, event)) = inner.read_ahead.take() else {
                break;
            };
            let input_event = InputEvent {
                time: KernelTimeval {
                    tv_sec: time.as_secs() as _,
                    tv_usec: time.subsec_micros() as _,
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
        match cmd {
            EVIOCGVERSION => {
                UserPtr::<u32>::from(arg).write_vm(0x10001)?;
                Ok(0)
            }
            EVIOCGID => {
                let inner = self.inner.lock();
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
                                let inner = self.inner.lock();
                                let device = inner.device.as_ref().ok_or(KError::NoSuchDevice)?;
                                return return_str(arg, size, device.name());
                            }
                            // EVIOCGPHYS
                            0x07 => {
                                let inner = self.inner.lock();
                                let device = inner.device.as_ref().ok_or(KError::NoSuchDevice)?;
                                let value = device.physical_location().to_string();
                                return return_str(arg, size, &value);
                            }
                            // EVIOCGUNIQ
                            0x08 => {
                                let inner = self.inner.lock();
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
                                let key_bits = self.inner.lock().key_state.as_bytes().to_vec();
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

    fn poll(&self, _file: &VfsFile) -> IoEvents {
        Pollable::poll(self)
    }

    fn register_poll(&self, _file: &VfsFile, context: &mut Context<'_>, events: IoEvents) {
        Pollable::register(self, context, events);
    }
}

impl Pollable for EventDev {
    fn poll(&self) -> IoEvents {
        let mut events = IoEvents::empty();
        events.set(IoEvents::IN, self.inner.lock().has_event());
        events
    }

    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        if events.contains(IoEvents::IN) {
            context.waker().wake_by_ref();
        }
    }
}

pub fn input_devices(fs: Arc<SimpleFs>) -> DirMapping {
    let mut inputs = DirMapping::new();
    let mut input_id = 0;
    let input_devices = inputdev::input_drain_devices();
    let mut keys = [0; 0x300usize.div_ceil(8)];
    for (i, device) in input_devices.into_iter().enumerate() {
        assert!(device.get_event_bits(EventType::Key, &mut keys).unwrap());

        let event_dev = Arc::new(EventDev::new(device));
        event_dev.subscribe_removed();

        let dev = DeviceFile::new(
            fs.clone(),
            NodeType::CharacterDevice,
            DeviceId::new(13, (i + 1) as _),
            event_dev,
        );

        const BTN_MOUSE: usize = 0x110;
        if keys[BTN_MOUSE / 8] & (1 << (BTN_MOUSE % 8)) != 0 {
            // Mouse
            add_device_entry(&mut inputs, "mice", dev);
        } else {
            add_device_entry(&mut inputs, format!("event{input_id}"), dev);
            input_id += 1;
        }
    }
    inputs
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    root.add(
        "input",
        SimpleDir::new_maker(fs.clone(), Arc::new(input_devices(fs.clone()))),
    );
}
