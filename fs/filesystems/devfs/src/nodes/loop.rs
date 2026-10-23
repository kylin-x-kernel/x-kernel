// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, sync::Arc, vec::Vec};
use core::{
    mem::{MaybeUninit, size_of},
    slice,
};

use kerrno::{KError, KResult, LinuxError};
use klazy::Lazy;
use ksync::{Mutex, static_lock};
use kvfs::{FMode, NodeType, VfsFile, VfsResult};
use linux_raw_sys::loop_device::{
    LO_FLAGS_READ_ONLY, LOOP_CLR_FD, LOOP_GET_STATUS, LOOP_SET_FD, LOOP_SET_STATUS, loop_info,
};
use osvm::VirtMutPtr;

static_lock! {
    static LOOP_VALIDATE_MUTEX: Mutex<()> = Mutex::new(());
}

/// State owned by one loop disk.
pub(crate) struct LoopDevice {
    number: u32,
    /// Underlying file for the loop device, if any.
    pub file: Mutex<Option<Arc<VfsFile>>>,
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
        }
    }

    /// Returns the current loop configuration.
    pub(crate) fn get_info(&self, block_device: &block::BlockDevice) -> KResult<loop_info> {
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
    pub(crate) fn set_info(&self, _src: loop_info) -> KResult<()> {
        Ok(())
    }

    /// Returns the currently bound backing file.
    pub(crate) fn clone_file(&self) -> VfsResult<Arc<VfsFile>> {
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
        has_sys_admin: bool,
    ) -> VfsResult<usize> {
        match cmd {
            LOOP_SET_FD => {
                let fd = arg as i32;
                if fd < 0 {
                    return Err(KError::BadFileDescriptor);
                }
                let _validation = LOOP_VALIDATE_MUTEX.lock();
                let file = kprocess::current_files().get_file(fd)?;
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
                block_device.disk().inc_diskseq();
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
                block_device.disk().inc_diskseq();
            }
            LOOP_GET_STATUS => {
                (arg as *mut loop_info).write_vm(self.get_info(block_device)?)?;
            }
            LOOP_SET_STATUS => {
                if !mode.contains(block::BlockOpenMode::WRITE) && !has_sys_admin {
                    return Err(KError::from(LinuxError::EPERM));
                }
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

    fn ioctl(
        &self,
        device: &block::BlockDevice,
        mode: block::BlockOpenMode,
        cmd: u32,
        arg: usize,
        has_sys_admin: bool,
    ) -> KResult<usize> {
        self.ioctl_impl(device, mode, cmd, arg, has_sys_admin)
    }
}

// Linux keeps loop instances independently of devtmpfs dentries. The block
// objects are therefore created once and each devfs mount only projects them.
static LOOP_DEVICES: Lazy<Vec<Arc<LoopDevice>>> = Lazy::new(|| {
    (0..16)
        .map(|number| {
            let device = Arc::new(LoopDevice::new(number));
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Loop devices use a fixed valid \
                          block size and initially zero capacity"
            )]
            let disk =
                block::Gendisk::alloc_mq(format!("loop{number}"), 7, number, 1, device.clone())
                    .expect("valid loop gendisk");
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: The once-initialized loop array \
                          uniquely owns major 7 minors 0 through 15"
            )]
            block::add_disk(disk).expect("loop device number must be unique");
            device
        })
        .collect()
});

pub(crate) fn init_devices() {
    Lazy::force(&LOOP_DEVICES);
}

impl block::BlkMqOperations for LoopDevice {
    fn queue_rq(&self, mut request: block::Request) {
        let file = self.file.lock().clone();
        let result = match file {
            None if request.operation() == block::BioOperation::Flush => Ok(()),
            None => Err(block::DriverError::BadState),
            Some(file) => loop_transfer(&mut request, &file),
        };
        request.complete(result);
    }
}

fn loop_transfer(request: &mut block::Request, file: &VfsFile) -> block::DriverResult {
    let operation = request.operation();
    if operation == block::BioOperation::Flush {
        return file.fsync(false).map_err(|_| block::DriverError::Io);
    }
    let mut position = request
        .sector()
        .checked_mul(512)
        .ok_or(block::DriverError::InvalidInput)?;
    let mut buffers = RequestIter::new(request);
    let expected = buffers.remaining;
    let count = match operation {
        block::BioOperation::Read => {
            file.read_iter_from(&mut iov_iter::iov_iter_dest(&mut buffers), &mut position)
        }
        block::BioOperation::Write => {
            file.write_iter_from(&mut iov_iter::iov_iter_source(&mut buffers), &mut position)
        }
        block::BioOperation::Flush => unreachable!(),
    }
    .map_err(|_| block::DriverError::Io)?;
    if count == expected {
        Ok(())
    } else {
        Err(block::DriverError::Io)
    }
}

// Borrowed bvec cursor for a single backing-file operation. The request retains
// every owner; only slice descriptors are collected, never a payload-sized copy.
struct RequestIter<'a> {
    vectors: Vec<block::BioVecRef<'a>>,
    index: usize,
    offset: usize,
    remaining: usize,
}

impl<'a> RequestIter<'a> {
    fn new(request: &'a mut block::Request) -> Self {
        let vectors: Vec<_> = request.vectors_mut().collect();
        let remaining = vectors.iter().map(|vector| vector.bytes().len()).sum();
        Self {
            vectors,
            index: 0,
            offset: 0,
            remaining,
        }
    }

    fn advance(&mut self, count: usize) {
        self.offset += count;
        self.remaining -= count;
        if self.offset == self.vectors[self.index].bytes().len() {
            self.index += 1;
            self.offset = 0;
        }
    }

    fn rewind(&mut self, count: usize) -> KResult<()> {
        let (mut index, mut offset, mut left) = (self.index, self.offset, count);
        while left > offset {
            left -= offset;
            index = index.checked_sub(1).ok_or(KError::InvalidInput)?;
            offset = self.vectors[index].bytes().len();
        }
        self.index = index;
        self.offset = offset - left;
        self.remaining += count;
        Ok(())
    }
}

impl iov_iter::IovSource for RequestIter<'_> {
    fn count(&self) -> usize {
        self.remaining
    }

    fn copy_from_iter(&mut self, dst: &mut [u8]) -> KResult<usize> {
        let mut copied = 0;
        while copied < dst.len() && self.remaining != 0 {
            let bytes = &self.vectors[self.index].bytes()[self.offset..];
            let count = bytes.len().min(dst.len() - copied);
            dst[copied..copied + count].copy_from_slice(&bytes[..count]);
            copied += count;
            self.advance(count);
        }
        Ok(copied)
    }

    fn revert(&mut self, count: usize) -> KResult<()> {
        self.rewind(count)
    }
}

impl iov_iter::IovSink for RequestIter<'_> {
    fn count(&self) -> usize {
        self.remaining
    }

    fn copy_to_iter(&mut self, src: &[u8]) -> KResult<usize> {
        let mut copied = 0;
        while copied < src.len() && self.remaining != 0 {
            let bytes = &mut self.vectors[self.index]
                .bytes_mut()
                .map_err(|_| KError::InvalidInput)?[self.offset..];
            let count = bytes.len().min(src.len() - copied);
            bytes[..count].copy_from_slice(&src[copied..copied + count]);
            copied += count;
            self.advance(count);
        }
        Ok(copied)
    }

    fn revert(&mut self, count: usize) -> KResult<()> {
        self.rewind(count)
    }
}

unittest::path_mod_test!(tests, "../../ut-tests/loop.rs");
