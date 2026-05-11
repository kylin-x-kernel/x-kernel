// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File metadata types shared by file-like objects.

use core::time::Duration;

use kvfs::DeviceId;
use linux_raw_sys::general::{
    S_IFMT, S_IFREG, STATX_ATTR_WRITE_ATOMIC, STATX_WRITE_ATOMIC, stat, statx, statx_timestamp,
};

/// Kernel stat structure containing file metadata.
#[derive(Debug, Clone, Copy)]
pub struct Kstat {
    pub dev: u64,
    pub ino: u64,
    pub nlink: u32,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub blksize: u32,
    pub blocks: u64,
    pub rdev: DeviceId,
    pub atime: Duration,
    pub mtime: Duration,
    pub ctime: Duration,
}

impl Default for Kstat {
    fn default() -> Self {
        Self {
            dev: 0,
            ino: 1,
            nlink: 1,
            mode: 0,
            uid: 1,
            gid: 1,
            size: 0,
            blksize: 4096,
            blocks: 0,
            rdev: DeviceId::default(),
            atime: Duration::default(),
            mtime: Duration::default(),
            ctime: Duration::default(),
        }
    }
}

impl From<Kstat> for stat {
    fn from(value: Kstat) -> Self {
        // SAFETY: `stat` is a POD (plain-old-data) struct from `linux_raw_sys`. All-zeroes
        // is a valid initial state for every field (integers become 0, pointers become null).
        let mut stat: stat = unsafe { core::mem::zeroed() };
        stat.st_dev = value.dev as _;
        stat.st_ino = value.ino as _;
        stat.st_nlink = value.nlink as _;
        stat.st_mode = value.mode as _;
        stat.st_uid = value.uid as _;
        stat.st_gid = value.gid as _;
        stat.st_size = value.size as _;
        stat.st_blksize = value.blksize as _;
        stat.st_blocks = value.blocks as _;
        stat.st_rdev = value.rdev.0 as _;

        stat.st_atime = value.atime.as_secs() as _;
        stat.st_atime_nsec = value.atime.subsec_nanos() as _;
        stat.st_mtime = value.mtime.as_secs() as _;
        stat.st_mtime_nsec = value.mtime.subsec_nanos() as _;
        stat.st_ctime = value.ctime.as_secs() as _;
        stat.st_ctime_nsec = value.ctime.subsec_nanos() as _;
        stat
    }
}

impl From<Kstat> for statx {
    fn from(value: Kstat) -> Self {
        const ATOMIC_WRITE_UNIT: u32 = 4096;

        // SAFETY: `statx` is a POD struct from `linux_raw_sys`. All-zeroes is a valid
        // initial state for every field (integers become 0, reserved fields become 0).
        let mut statx: statx = unsafe { core::mem::zeroed() };
        statx.stx_blksize = value.blksize as _;
        statx.stx_attributes = 0;
        statx.stx_attributes_mask = STATX_ATTR_WRITE_ATOMIC as _;
        statx.stx_nlink = value.nlink as _;
        statx.stx_uid = value.uid as _;
        statx.stx_gid = value.gid as _;
        statx.stx_mode = value.mode as _;
        statx.stx_ino = value.ino as _;
        statx.stx_size = value.size as _;
        statx.stx_blocks = value.blocks as _;
        statx.stx_rdev_major = value.rdev.major();
        statx.stx_rdev_minor = value.rdev.minor();

        fn time_to_statx(time: &Duration) -> statx_timestamp {
            statx_timestamp {
                tv_sec: time.as_secs() as _,
                tv_nsec: time.subsec_nanos() as _,
                __reserved: 0,
            }
        }
        statx.stx_atime = time_to_statx(&value.atime);
        statx.stx_ctime = time_to_statx(&value.ctime);
        statx.stx_mtime = time_to_statx(&value.mtime);

        statx.stx_dev_major = (value.dev >> 32) as _;
        statx.stx_dev_minor = value.dev as _;

        if value.mode & S_IFMT == S_IFREG {
            statx.stx_attributes |= STATX_ATTR_WRITE_ATOMIC as u64;
            statx.stx_atomic_write_unit_min = ATOMIC_WRITE_UNIT;
            statx.stx_atomic_write_unit_max = ATOMIC_WRITE_UNIT;
            statx.stx_atomic_write_unit_max_opt = ATOMIC_WRITE_UNIT;
            statx.stx_atomic_write_segments_max = 1;
            statx.stx_mask |= STATX_WRITE_ATOMIC;
        }

        statx
    }
}
