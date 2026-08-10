// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{boxed::Box, format, sync::Arc, vec::Vec};
use core::{
    mem::{MaybeUninit, size_of},
    slice,
    sync::atomic::{AtomicU32, Ordering},
};

use kerrno::{KError, KResult, LinuxError};
use klazy::Lazy;
use ksync::Mutex;
use kvfs::{FMode, NodeType, VfsFile, VfsResult};
use linux_raw_sys::{
    ioctl::{BLKRAGET, BLKRASET},
    loop_device::{
        LO_FLAGS_READ_ONLY, LOOP_CLR_FD, LOOP_GET_STATUS, LOOP_SET_FD, LOOP_SET_STATUS, loop_info,
    },
};
use osvm::{VirtMutPtr, VirtPtr};

static LOOP_VALIDATE_MUTEX: Mutex<()> = Mutex::new(());

/// State owned by one loop disk.
pub struct LoopDevice {
    number: u32,
    /// Underlying file for the loop device, if any.
    pub file: Mutex<Option<Arc<VfsFile>>>,
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
    /// Creates an unbound loop device.
    pub(crate) fn new(number: u32) -> Self {
        Self {
            number,
            file: Mutex::new(None),
            ra: AtomicU32::new(512),
        }
    }

    /// Returns the current loop configuration.
    pub fn get_info(&self, block_device: &block::BlockDevice) -> KResult<loop_info> {
        if self.file.lock().is_none() {
            return Err(KError::from(LinuxError::ENXIO));
        }
        Ok(loop_info {
            lo_number: self.number as _,
            lo_device: 0,
            lo_inode: 0,
            lo_rdevice: 0,
            lo_offset: 0,
            lo_encrypt_type: 0,
            lo_encrypt_key_size: 0,
            lo_flags: if block_device.is_read_only() {
                LO_FLAGS_READ_ONLY as i32
            } else {
                0
            },
            lo_name: [0; 64],
            lo_encrypt_key: [0; 32],
            lo_init: [0; 2],
            reserved: [0; 4],
        })
    }

    /// Applies loop configuration fields supported by this driver.
    pub fn set_info(&self, _src: loop_info) -> KResult<()> {
        Ok(())
    }

    /// Returns the currently bound backing file.
    pub fn clone_file(&self) -> VfsResult<Arc<VfsFile>> {
        self.file
            .lock()
            .clone()
            .ok_or(KError::from(LinuxError::ENXIO))
    }

    fn validate_backing_file(
        &self,
        file: &Arc<VfsFile>,
        block_device: &block::BlockDevice,
    ) -> VfsResult<()> {
        let mut current = file.clone();
        loop {
            if !matches!(
                current.node_type(),
                NodeType::RegularFile | NodeType::BlockDevice
            ) {
                return Err(KError::InvalidInput);
            }
            if current.node_type() != NodeType::BlockDevice || current.inode().rdev().major() != 7 {
                return Ok(());
            }
            if current.inode().rdev() == block_device.device_number() {
                return Err(KError::BadFileDescriptor);
            }
            let number = usize::try_from(current.inode().rdev().minor())
                .map_err(|_| KError::InvalidInput)?;
            current = LOOP_DEVICES
                .get(number)
                .ok_or(KError::InvalidInput)?
                .clone_file()
                .map_err(|_| KError::InvalidInput)?;
        }
    }

    fn ioctl_impl(
        &self,
        block_device: &block::BlockDevice,
        mode: block::BlockOpenMode,
        cmd: u32,
        arg: usize,
    ) -> VfsResult<usize> {
        match cmd {
            LOOP_SET_FD => {
                let fd = arg as i32;
                if fd < 0 {
                    return Err(KError::BadFileDescriptor);
                }
                let _validation = LOOP_VALIDATE_MUTEX.lock();
                let file = kprocess::current_resources().get_file(fd)?;
                self.validate_backing_file(&file, block_device)?;
                let capacity = file.size() / 512;
                let read_only = !file.mode().contains(FMode::WRITE)
                    || !mode.contains(block::BlockOpenMode::WRITE);
                let mut backing = self.file.lock();
                if backing.is_some() {
                    return Err(KError::ResourceBusy);
                }
                let old_read_only = block_device.is_read_only();
                block_device.set_disk_read_only(read_only)?;
                *backing = Some(file);
                if block_device.set_capacity(capacity).is_err() {
                    *backing = None;
                    block_device.set_disk_read_only(old_read_only)?;
                    return Err(KError::InvalidInput);
                }
            }
            LOOP_CLR_FD => {
                let _validation = LOOP_VALIDATE_MUTEX.lock();
                let mut backing = self.file.lock();
                if backing.is_none() {
                    return Err(KError::from(LinuxError::ENXIO));
                }
                block_device
                    .set_capacity(0)
                    .map_err(|_| KError::InvalidInput)?;
                *backing = None;
                block_device.set_disk_read_only(false)?;
            }
            LOOP_GET_STATUS => {
                (arg as *mut loop_info).write_vm(self.get_info(block_device)?)?;
            }
            LOOP_SET_STATUS => {
                let mut info = empty_loop_info();
                // SAFETY: `info` is a live POD value and the byte slice covers
                // exactly its initialized storage for a copy-from-user.
                let info_bytes = unsafe {
                    slice::from_raw_parts_mut(
                        (&mut info as *mut loop_info).cast::<MaybeUninit<u8>>(),
                        size_of::<loop_info>(),
                    )
                };
                osvm::read_vm_bytes(arg as *const u8, info_bytes)?;
                self.set_info(info)?;
            }
            BLKRAGET => {
                (arg as *mut u32).write_vm(self.ra.load(Ordering::Relaxed))?;
            }
            BLKRASET => {
                self.ra
                    .store((arg as *const u32).read_vm()?, Ordering::Relaxed);
            }
            _ => return Err(KError::NotATty),
        }
        Ok(0)
    }
}

impl block::BlockDeviceOperations for LoopDevice {
    fn num_blocks(&self) -> u64 {
        self.file
            .lock()
            .as_ref()
            .map_or(0, |file| file.size() / 512)
    }

    fn block_size(&self) -> usize {
        512
    }

    fn read_block(&self, block_id: u64, buf: &mut [u8]) -> block::DriverResult<()> {
        let offset = block_id
            .checked_mul(512)
            .ok_or(block::DriverError::InvalidInput)?;
        let file = self
            .clone_file()
            .map_err(|_| block::DriverError::BadState)?;
        let mut position = offset;
        let read = file
            .read_from(buf, &mut position)
            .map_err(|_| block::DriverError::Io)?;
        if read != buf.len() {
            return Err(block::DriverError::Io);
        }
        Ok(())
    }

    fn write_block(&self, block_id: u64, buf: &[u8]) -> block::DriverResult<()> {
        let offset = block_id
            .checked_mul(512)
            .ok_or(block::DriverError::InvalidInput)?;
        let file = self
            .clone_file()
            .map_err(|_| block::DriverError::BadState)?;
        let mut position = offset;
        let written = file
            .write_from(buf, &mut position)
            .map_err(|_| block::DriverError::Io)?;
        if written != buf.len() {
            return Err(block::DriverError::Io);
        }
        Ok(())
    }

    fn flush(&self) -> block::DriverResult<()> {
        if let Some(file) = self.file.lock().as_ref() {
            file.fsync(false).map_err(|_| block::DriverError::Io)?;
        }
        Ok(())
    }

    fn ioctl(
        &self,
        device: &block::BlockDevice,
        mode: block::BlockOpenMode,
        cmd: u32,
        arg: usize,
    ) -> KResult<usize> {
        self.ioctl_impl(device, mode, cmd, arg)
    }
}

// Linux keeps loop instances independently of devtmpfs dentries. The block
// objects are therefore created once and each devfs mount only projects them.
static LOOP_DEVICES: Lazy<Vec<Arc<LoopDevice>>> = Lazy::new(|| {
    (0..16)
        .map(|number| {
            let device = Arc::new(LoopDevice::new(number));
            let disk = block::Gendisk::new(
                format!("loop{number}"),
                7,
                number,
                1,
                Box::new(device.clone()),
            )
            .expect("valid loop gendisk");
            block::add_disk(Arc::new(disk)).expect("loop device number must be unique");
            device
        })
        .collect()
});

pub(crate) fn init_devices() {
    Lazy::force(&LOOP_DEVICES);
}
