// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Generic Linux block ioctl dispatch, independent of host user-memory access.

use kerrno::{KError, KResult};
use linux_raw_sys::ioctl::{
    BLKALIGNOFF, BLKFRAGET, BLKFRASET, BLKGETDISKSEQ, BLKGETSIZE, BLKGETSIZE64, BLKIOMIN, BLKIOOPT,
    BLKPBSZGET, BLKRAGET, BLKRASET, BLKROGET, BLKROSET, BLKSSZGET,
};
use memaddr::PAGE_SIZE_4K;

use crate::{BlockDevice, BlockOpenMode};

impl BlockDevice {
    /// Handles generic block ioctls before dispatching device-specific commands.
    ///
    /// The host supplies fault-checked access to the argument: `read_int` reads
    /// one native-endian C `int`, and `write_arg` copies exactly the supplied
    /// bytes. Neither user-copy callback runs for an unknown command; its original
    /// `mode`, `cmd`, `arg`, and `has_sys_admin` are passed to the driver unchanged.
    ///
    /// The trusted host supplies `has_sys_admin` from the current caller's
    /// CAP_SYS_ADMIN capability, not the opening credential. The dispatcher and
    /// driver each enforce the permissions of the commands they own.
    /// Call in sleepable context: user copies and driver callbacks
    /// may sleep. No registry lock is held across either kind of callback.
    ///
    /// # Errors
    ///
    /// Propagates copy faults and driver errors. Administrative setters return
    /// [`KError::PermissionDenied`] without privilege. Readahead getters reject
    /// a null pointer with [`KError::InvalidInput`] before copying.
    /// `BLKGETSIZE` returns
    /// [`KError::FileTooLarge`] if the sector count does not fit a native unsigned
    /// long. Unsupported commands normally return [`KError::NotATty`].
    pub fn ioctl(
        &self,
        mode: BlockOpenMode,
        cmd: u32,
        arg: usize,
        has_sys_admin: bool,
        read_int: impl FnOnce() -> KResult<i32>,
        write_arg: impl FnOnce(&[u8]) -> KResult<()>,
    ) -> KResult<usize> {
        let limits = self.queue().limits();
        match cmd {
            BLKSSZGET => write_arg(&(limits.logical_block_size() as i32).to_ne_bytes())?,
            BLKPBSZGET => write_arg(&limits.physical_block_size().to_ne_bytes())?,
            BLKIOMIN => write_arg(&limits.io_min().to_ne_bytes())?,
            BLKIOOPT => write_arg(&limits.io_opt().to_ne_bytes())?,
            // Only whole-disk views exist; their offset is the queue offset.
            BLKALIGNOFF => write_arg(&(limits.alignment_offset() as i32).to_ne_bytes())?,
            BLKGETSIZE => {
                let sectors =
                    usize::try_from(self.num_sectors()).map_err(|_| KError::FileTooLarge)?;
                write_arg(&sectors.to_ne_bytes())?;
            }
            BLKGETSIZE64 => write_arg(&self.size().to_ne_bytes())?,
            BLKGETDISKSEQ => write_arg(&self.disk().diskseq().to_ne_bytes())?,
            BLKROGET => write_arg(&i32::from(self.is_read_only()).to_ne_bytes())?,
            BLKROSET => {
                if !has_sys_admin {
                    return Err(KError::PermissionDenied);
                }
                self.set_disk_read_only(read_int()? != 0)?;
            }
            BLKRASET | BLKFRASET => {
                if !has_sys_admin {
                    return Err(KError::PermissionDenied);
                }
                // Linux takes an immediate sector count, with native unsigned-long
                // wrapping, then rounds down to whole base pages.
                self.disk()
                    .backing_dev_info()
                    .set_readahead_pages(arg.wrapping_mul(512) / PAGE_SIZE_4K);
            }
            BLKRAGET | BLKFRAGET => {
                if arg == 0 {
                    return Err(KError::InvalidInput);
                }
                let sectors = self
                    .disk()
                    .backing_dev_info()
                    .readahead_pages()
                    .wrapping_mul(PAGE_SIZE_4K)
                    / 512;
                write_arg(&sectors.to_ne_bytes())?;
            }
            _ => return self.driver_ioctl(mode, cmd, arg, has_sys_admin),
        }
        Ok(0)
    }
}
