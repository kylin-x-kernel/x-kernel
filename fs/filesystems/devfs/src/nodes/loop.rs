// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, sync::Arc};
use core::{
    mem::MaybeUninit,
    slice,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use kerrno::{KError, KResult, LinuxError};
use ksync::Mutex;
use kvfs::{
    DeviceFileOps, DeviceId, DirMapping, MmapMapper, NodeFlags, NodeType, SimpleFs, VfsFile,
    VfsResult,
};
use linux_raw_sys::{
    ioctl::{BLKGETSIZE, BLKGETSIZE64, BLKRAGET, BLKRASET, BLKROGET, BLKROSET},
    loop_device::{LOOP_CLR_FD, LOOP_GET_STATUS, LOOP_SET_FD, LOOP_SET_STATUS, loop_info},
};
use osvm::{VirtMutPtr, VirtPtr};

use crate::{DeviceFile, add_device_entry};

/// /dev/loopX devices
/// Loop device for attaching regular files as block devices
pub struct LoopDevice {
    number: u32,
    dev_id: DeviceId,
    /// Underlying file for the loop device, if any.
    pub file: Mutex<Option<Arc<VfsFile>>>,
    /// Read-only flag for the loop device.
    pub ro: AtomicBool,
    /// Read-ahead size for the loop device, in bytes.
    pub ra: AtomicU32,
}

const fn empty_loop_info() -> loop_info {
    loop_info {
        lo_number: 0,
        lo_device: 0,
        lo_inode: 0,
        lo_rdevice: 0,
        lo_offset: 0,
        lo_encrypt_type: 0,
        lo_encrypt_key_size: 0,
        lo_flags: 0,
        lo_name: [0; 64],
        lo_encrypt_key: [0; 32],
        lo_init: [0; 2],
        reserved: [0; 4],
    }
}

impl LoopDevice {
    /// Create a new loop device
    pub(crate) fn new(number: u32, dev_id: DeviceId) -> Self {
        Self {
            number,
            dev_id,
            file: Mutex::new(None),
            ro: AtomicBool::new(false),
            ra: AtomicU32::new(512),
        }
    }

    /// Get information about the loop device.
    pub fn get_info(&self) -> KResult<loop_info> {
        if self.file.lock().is_none() {
            return Err(KError::from(LinuxError::ENXIO));
        }
        Ok(loop_info {
            lo_number: self.number as _,
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: self.dev_id.0 as _,
            lo_offset: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: 0,
            lo_name: [0; 64],
            lo_encrypt_key: [0; 32],
            lo_init: [0; 2],
            reserved: [0; 4],
        })
    }

    /// Set information for the loop device.
    pub fn set_info(&self, _src: loop_info) -> KResult<()> {
        Ok(())
    }

    /// Clone the underlying file of the loop device.
    pub fn clone_file(&self) -> VfsResult<Arc<VfsFile>> {
        let file = self.file.lock().clone();
        file.ok_or(KError::from(LinuxError::ENXIO))
    }
}

impl DeviceFileOps for LoopDevice {
    fn supports_read(&self) -> bool {
        true
    }

    fn supports_write(&self) -> bool {
        true
    }

    fn mmap(&self, _file: &VfsFile, mapper: &mut dyn MmapMapper) -> VfsResult<()> {
        match self.file.lock().as_ref() {
            Some(file) if file.is_regular_file() => mapper.map_file_backed(),
            _ => Err(kvfs::VfsError::NoSuchDevice),
        }
    }

    fn read(&self, _file: &VfsFile, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let file = self
            .clone_file()
            .map_err(|_| KError::OperationNotPermitted)?;
        let mut pos = offset;
        file.read_from(buf, &mut pos)
    }

    fn write(&self, _file: &VfsFile, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if self.ro.load(Ordering::Relaxed) {
            return Err(KError::ReadOnlyFilesystem);
        }
        let file = self
            .clone_file()
            .map_err(|_| KError::OperationNotPermitted)?;
        let mut pos = offset;
        file.write_from(buf, &mut pos)
    }

    fn ioctl(&self, _file: &VfsFile, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            LOOP_SET_FD => {
                let fd = arg as i32;
                if fd < 0 {
                    return Err(KError::BadFileDescriptor);
                }
                let file = kprocess::current_resources().get_file(fd)?;
                let mut guard = self.file.lock();
                if guard.is_some() {
                    return Err(KError::ResourceBusy);
                }

                *guard = Some(file);
            }
            LOOP_CLR_FD => {
                let mut guard = self.file.lock();
                if guard.is_none() {
                    return Err(KError::from(LinuxError::ENXIO));
                }
                *guard = None;
            }
            LOOP_GET_STATUS => {
                (arg as *mut loop_info).write_vm(self.get_info()?)?;
            }
            LOOP_SET_STATUS => {
                let mut info = empty_loop_info();
                // SAFETY: `info` is a live POD `loop_info` value, so its full
                // storage may be reborrowed as `MaybeUninit<u8>` for copy-from-user.
                let info_bytes = unsafe {
                    slice::from_raw_parts_mut(
                        (&mut info as *mut loop_info).cast::<MaybeUninit<u8>>(),
                        size_of::<loop_info>(),
                    )
                };
                osvm::read_vm_bytes(arg as *const u8, info_bytes)?;
                self.set_info(info)?;
            }
            // TODO: the following should apply to any block devices
            BLKGETSIZE | BLKGETSIZE64 => {
                let file = self.clone_file()?;
                let sectors = file.size() / 512;
                if cmd == BLKGETSIZE {
                    (arg as *mut u32).write_vm(sectors as _)?;
                } else {
                    (arg as *mut u64).write_vm(sectors * 512)?;
                }
            }
            BLKROGET => {
                (arg as *mut u32).write_vm(self.ro.load(Ordering::Relaxed) as u32)?;
            }
            BLKROSET => {
                let ro = (arg as *const u32).read_vm()?;
                if ro != 0 && ro != 1 {
                    return Err(KError::InvalidInput);
                }
                self.ro.store(ro != 0, Ordering::Relaxed);
            }
            BLKRAGET => {
                (arg as *mut u32).write_vm(self.ra.load(Ordering::Relaxed))?;
            }
            BLKRASET => {
                self.ra
                    .store((arg as *const u32).read_vm()? as _, Ordering::Relaxed);
            }
            _ => {
                warn!("unknown ioctl for loop device: {cmd}");
                return Err(KError::NotATty);
            }
        }
        Ok(0)
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}

pub(crate) fn add_root_entries(root: &mut DirMapping, fs: Arc<SimpleFs>) {
    for i in 0..16 {
        let dev_id = DeviceId::new(7, i);
        add_device_entry(
            root,
            format!("loop{i}"),
            DeviceFile::new(
                fs.clone(),
                NodeType::BlockDevice,
                dev_id,
                Arc::new(LoopDevice::new(i, dev_id)),
            ),
        );
    }
}
