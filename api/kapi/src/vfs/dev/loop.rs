use core::{
    any::Any,
    sync::atomic::{AtomicBool, AtomicU32, Ordering},
};

use fs_ng_vfs::{DeviceId, NodeFlags, VfsResult};
use kcore::vfs::{DeviceMmap, DeviceOps};
use kerrno::{KError, KResult, LinuxError};
use kfs::FileBackend;
use ksync::Mutex;
use linux_raw_sys::{
    ioctl::{BLKGETSIZE, BLKGETSIZE64, BLKRAGET, BLKRASET, BLKROGET, BLKROSET},
    loop_device::{LOOP_CLR_FD, LOOP_GET_STATUS, LOOP_SET_FD, LOOP_SET_STATUS, loop_info},
};
use osvm::{VirtMutPtr, VirtPtr};

use crate::file::get_file_like;

/// /dev/loopX devices
/// Loop device for attaching regular files as block devices
pub struct LoopDevice {
    number: u32,
    dev_id: DeviceId,
    /// Underlying file for the loop device, if any.
    pub file: Mutex<Option<FileBackend>>,
    /// Read-only flag for the loop device.
    pub ro: AtomicBool,
    /// Read-ahead size for the loop device, in bytes.
    pub ra: AtomicU32,
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
        let mut res: loop_info = unsafe { core::mem::zeroed() };
        res.lo_number = self.number as _;
        res.lo_rdevice = self.dev_id.0 as _;
        Ok(res)
    }

    /// Set information for the loop device.
    pub fn set_info(&self, _src: loop_info) -> KResult<()> {
        Ok(())
    }

    /// Clone the underlying file of the loop device.
    pub fn clone_file(&self) -> VfsResult<FileBackend> {
        let file = self.file.lock().clone();
        file.ok_or(KError::from(LinuxError::ENXIO))
    }
}

impl DeviceOps for LoopDevice {
    fn read_at(&self, buf: &mut [u8], offset: u64) -> VfsResult<usize> {
        let file = self.file.lock().clone();
        file.ok_or(KError::OperationNotPermitted)?
            .read_at(buf, offset)
    }

    fn write_at(&self, buf: &[u8], offset: u64) -> VfsResult<usize> {
        if self.ro.load(Ordering::Relaxed) {
            return Err(KError::ReadOnlyFilesystem);
        }
        let file = self.file.lock().clone();
        file.ok_or(KError::OperationNotPermitted)?
            .write_at(buf, offset)
    }

    fn ioctl(&self, cmd: u32, arg: usize) -> VfsResult<usize> {
        match cmd {
            LOOP_SET_FD => {
                let fd = arg as i32;
                if fd < 0 {
                    return Err(KError::BadFileDescriptor);
                }
                let f = get_file_like(fd)?;
                let Some(file) = f.downcast_ref::<crate::file::File>() else {
                    return Err(KError::InvalidInput);
                };
                let mut guard = self.file.lock();
                if guard.is_some() {
                    return Err(KError::ResourceBusy);
                }

                *guard = Some(file.inner().backend()?.clone());
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
                // FIXME: AnyBitPattern
                let info = unsafe { (arg as *const loop_info).read_uninit()?.assume_init() };
                self.set_info(info)?;
            }
            // TODO: the following should apply to any block devices
            BLKGETSIZE | BLKGETSIZE64 => {
                let file = self.clone_file()?;
                let sectors = file.location().len()? / 512;
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

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn mmap(&self) -> DeviceMmap {
        if let Some(FileBackend::Cached(cache)) = self.file.lock().as_ref() {
            DeviceMmap::Cache(cache.clone())
        } else {
            DeviceMmap::None
        }
    }

    fn flags(&self) -> NodeFlags {
        NodeFlags::NON_CACHEABLE
    }
}
