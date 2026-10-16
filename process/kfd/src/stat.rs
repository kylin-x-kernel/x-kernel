// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File metadata types shared by descriptor-backed files.

use ktime_types::SystemTime;
use kvfs::DeviceId;
use linux_raw_sys::general::{
    S_IFMT, S_IFREG, STATX_ATTR_WRITE_ATOMIC, STATX_BASIC_STATS, STATX_WRITE_ATOMIC, stat, statx,
    statx_timestamp,
};

/// Kernel stat structure containing file metadata.
#[derive(Debug, Clone, Copy)]
pub struct Kstat {
    /// Device identifier containing this inode.
    pub dev: u64,
    /// Inode number.
    pub ino: u64,
    /// Link count.
    pub nlink: u32,
    /// File type and permission bits.
    pub mode: u32,
    /// Owner user ID.
    pub uid: u32,
    /// Owner group ID.
    pub gid: u32,
    /// File size in bytes.
    pub size: u64,
    /// Preferred block size for I/O.
    pub blksize: u32,
    /// Number of allocated blocks.
    pub blocks: u64,
    /// Device ID represented by this inode, for special files.
    pub rdev: DeviceId,
    /// Last access time.
    pub atime: SystemTime,
    /// Last modification time.
    pub mtime: SystemTime,
    /// Last metadata change time.
    pub ctime: SystemTime,
}

impl From<kvfs::Metadata> for Kstat {
    fn from(metadata: kvfs::Metadata) -> Self {
        let mode = u32::from(metadata.mode.bits());
        Self {
            dev: metadata.device,
            ino: metadata.inode,
            mode,
            nlink: metadata.nlink as _,
            uid: metadata.uid,
            gid: metadata.gid,
            size: metadata.size,
            blksize: metadata.block_size as _,
            blocks: metadata.blocks,
            rdev: metadata.rdev,
            atime: metadata.atime,
            mtime: metadata.mtime,
            ctime: metadata.ctime,
        }
    }
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
            atime: SystemTime::UNIX_EPOCH,
            mtime: SystemTime::UNIX_EPOCH,
            ctime: SystemTime::UNIX_EPOCH,
        }
    }
}

impl From<Kstat> for stat {
    fn from(value: Kstat) -> Self {
        #[expect(
            clippy::disallowed_methods,
            reason = "CG-018-uninitialized-storage-model: linux_raw_sys::general::stat contains \
                      only integers and integer arrays, which admit all-zero bits; zeroing also \
                      covers its architecture-specific reserved fields"
        )]
        // SAFETY: The Linux `stat` bindings contain only integers and integer
        // arrays on every supported architecture, so all-zero bits are valid.
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

        stat.st_atime = value.atime.unix_seconds() as _;
        stat.st_atime_nsec = value.atime.subsec_nanos() as _;
        stat.st_mtime = value.mtime.unix_seconds() as _;
        stat.st_mtime_nsec = value.mtime.subsec_nanos() as _;
        stat.st_ctime = value.ctime.unix_seconds() as _;
        stat.st_ctime_nsec = value.ctime.subsec_nanos() as _;
        stat
    }
}

impl From<Kstat> for statx {
    fn from(value: Kstat) -> Self {
        const ATOMIC_WRITE_UNIT: u32 = 4096;

        fn time_to_statx(time: &SystemTime) -> statx_timestamp {
            statx_timestamp {
                tv_sec: time.unix_seconds() as _,
                tv_nsec: time.subsec_nanos() as _,
                __reserved: 0,
            }
        }
        let mut statx = statx {
            stx_mask: STATX_BASIC_STATS,
            stx_blksize: value.blksize as _,
            stx_attributes: 0,
            stx_nlink: value.nlink as _,
            stx_uid: value.uid as _,
            stx_gid: value.gid as _,
            stx_mode: value.mode as _,
            __spare0: [0; 1],
            stx_ino: value.ino as _,
            stx_size: value.size as _,
            stx_blocks: value.blocks as _,
            stx_attributes_mask: STATX_ATTR_WRITE_ATOMIC as _,
            stx_atime: time_to_statx(&value.atime),
            stx_btime: time_to_statx(&SystemTime::UNIX_EPOCH),
            stx_ctime: time_to_statx(&value.ctime),
            stx_mtime: time_to_statx(&value.mtime),
            stx_rdev_major: value.rdev.major(),
            stx_rdev_minor: value.rdev.minor(),
            stx_dev_major: (value.dev >> 32) as _,
            stx_dev_minor: value.dev as _,
            stx_mnt_id: 0,
            stx_dio_mem_align: 0,
            stx_dio_offset_align: 0,
            stx_subvol: 0,
            stx_atomic_write_unit_min: 0,
            stx_atomic_write_unit_max: 0,
            stx_atomic_write_segments_max: 0,
            stx_dio_read_offset_align: 0,
            stx_atomic_write_unit_max_opt: 0,
            __spare2: [0; 1],
            __spare3: [0; 8],
        };

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

unittest::path_mod_test!(tests, "../ut-tests/stat.rs");
