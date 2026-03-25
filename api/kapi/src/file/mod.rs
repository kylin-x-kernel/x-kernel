// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File descriptor abstractions and file-like traits.

pub mod epoll;
pub mod event;
mod fs;
mod net;
mod pidfd;
mod pipe;
pub mod signalfd;

use alloc::{borrow::Cow, sync::Arc};
use core::{ffi::c_int, time::Duration};

use downcast_rs::{DowncastSync, impl_downcast};
use flatten_objects::FlattenObjects;
use fs_ng_vfs::DeviceId;
use kcore::{resources::FILE_LIMIT, task::AsThread};
use kerrno::{KError, KResult};
use kfs::{FS_CONTEXT, OpenOptions};
use kio::prelude::*;
use kpoll::Pollable;
use ksync::RwLock;
use ktask::current;
use linux_raw_sys::general::{
    RLIMIT_NOFILE, S_IFMT, S_IFREG, STATX_ATTR_WRITE_ATOMIC, STATX_WRITE_ATOMIC, stat, statx,
    statx_timestamp,
};

pub use self::{
    fs::{Directory, File, ResolveAtResult, metadata_to_kstat, resolve_at, with_fs},
    net::Socket,
    pidfd::PidFd,
    pipe::Pipe,
};

/// Kernel stat structure containing file metadata.
///
/// This structure mirrors the POSIX `stat` structure and contains information about a file,
/// including device IDs, inode number, permissions, sizes, and timestamps.
#[derive(Debug, Clone, Copy)]
pub struct Kstat {
    /// Device ID of the filesystem containing the file.
    pub dev: u64,
    /// Inode number.
    pub ino: u64,
    /// Number of hard links.
    pub nlink: u32,
    /// File mode and permissions.
    pub mode: u32,
    /// Owner user ID.
    pub uid: u32,
    /// Owner group ID.
    pub gid: u32,
    /// File size in bytes.
    pub size: u64,
    /// Preferred I/O block size.
    pub blksize: u32,
    /// Number of allocated blocks.
    pub blocks: u64,
    /// Device ID for special files.
    pub rdev: DeviceId,
    /// Last access time.
    pub atime: Duration,
    /// Last modification time.
    pub mtime: Duration,
    /// Last status change time.
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
        // SAFETY: valid for stat
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

        // SAFETY: valid for statx
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

/// Trait for types that can be used as write destinations in I/O operations.
pub trait WriteBuf: Write + IoBufMut {}
impl<T: Write + IoBufMut> WriteBuf for T {}
/// I/O destination buffer type for write operations.
pub type IoDst<'a> = dyn WriteBuf + 'a;

/// Trait for types that can be used as read sources in I/O operations.
pub trait ReadBuf: Read + IoBuf {}
impl<T: Read + IoBuf> ReadBuf for T {}
/// I/O source buffer type for read operations.
pub type IoSrc<'a> = dyn ReadBuf + 'a;

/// Trait for file-like objects that support standard file operations.
///
/// This trait abstracts various file-like objects (regular files, directories, sockets, pipes, etc.)
/// and provides a unified interface for I/O, metadata retrieval, and control operations.
#[allow(dead_code)]
pub trait FileLike: Pollable + DowncastSync {
    /// Reads data from the file into the provided buffer.
    fn read(&self, _dst: &mut IoDst) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    /// Writes data from the provided buffer to the file.
    fn write(&self, _src: &mut IoSrc) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    /// Gets file metadata and statistics.
    fn stat(&self) -> KResult<Kstat> {
        Ok(Kstat::default())
    }

    /// Returns the absolute path of this file.
    fn path(&self) -> Cow<'_, str>;

    /// Performs I/O control operations.
    fn ioctl(&self, _cmd: u32, _arg: usize) -> KResult<usize> {
        Err(KError::NotATty)
    }

    /// Returns whether this file is in non-blocking mode.
    fn nonblocking(&self) -> bool {
        false
    }

    /// Sets or clears the non-blocking flag.
    fn set_nonblocking(&self, _nonblocking: bool) -> KResult {
        Ok(())
    }

    /// Converts a file descriptor to a file-like reference.
    fn from_fd(fd: c_int) -> KResult<Arc<Self>>
    where
        Self: Sized + 'static,
    {
        get_file_like(fd)?
            .downcast_arc()
            .map_err(|_| KError::InvalidInput)
    }

    /// Adds this file-like object to the current process's file descriptor table.
    fn add_to_fd_table(self, cloexec: bool) -> KResult<c_int>
    where
        Self: Sized + 'static,
    {
        add_file_like(Arc::new(self), cloexec)
    }
}
impl_downcast!(sync FileLike);

/// A file descriptor entry in the file descriptor table.
///
/// Contains a reference to the file-like object and flags like close-on-exec.
#[derive(Clone)]
pub struct FileDescriptor {
    pub inner: Arc<dyn FileLike>,
    /// Close-on-exec flag (true if file should be closed on exec)
    pub cloexec: bool,
}

fn new_fd_table() -> Arc<RwLock<FlattenObjects<FileDescriptor, { FILE_LIMIT }>>> {
    let mut table = Arc::<RwLock<FlattenObjects<FileDescriptor, { FILE_LIMIT }>>>::new_uninit();
    Arc::get_mut(&mut table)
        .expect("newly allocated Arc must be unique")
        .write(RwLock::new(FlattenObjects::new()));
    unsafe { table.assume_init() }
}

scope_local::scope_local! {
    /// The current file descriptor table.
    pub static FD_TABLE: Arc<RwLock<FlattenObjects<FileDescriptor, { FILE_LIMIT }>>> = new_fd_table();
}

/// Retrieves a file-like object from the file descriptor table.
///
/// # Arguments
/// - `fd`: The file descriptor to look up
///
/// # Returns
/// A reference to the file-like object, or `BadFileDescriptor` error if not found.
pub fn get_file_like(fd: c_int) -> KResult<Arc<dyn FileLike>> {
    FD_TABLE
        .read()
        .get(fd as usize)
        .map(|fd| fd.inner.clone())
        .ok_or(KError::BadFileDescriptor)
}

/// Adds a file-like object to the current process's file descriptor table.
///
/// # Arguments
/// - `f`: The file-like object to add
/// - `cloexec`: Whether to set the close-on-exec flag
///
/// # Returns
/// The new file descriptor number, or an error if the table is full.
pub fn add_file_like(f: Arc<dyn FileLike>, cloexec: bool) -> KResult<c_int> {
    let max_nofile = current().as_thread().proc_data.rlim.read()[RLIMIT_NOFILE].current;
    let mut table = FD_TABLE.write();
    if table.count() as u64 >= max_nofile {
        return Err(KError::TooManyOpenFiles);
    }
    let fd = FileDescriptor { inner: f, cloexec };
    Ok(table.add(fd).map_err(|_| KError::TooManyOpenFiles)? as c_int)
}

/// Closes a file descriptor and removes it from the file descriptor table.
///
/// # Arguments
/// - `fd`: The file descriptor to close
pub fn close_file_like(fd: c_int) -> KResult {
    let f = FD_TABLE
        .write()
        .remove(fd as usize)
        .ok_or(KError::BadFileDescriptor)?;
    debug!("close_file_like <= count: {}", Arc::strong_count(&f.inner));
    Ok(())
}

#[cfg(unittest)]
mod kstat_tests {
    use unittest::def_test;

    use super::*;

    #[def_test]
    fn test_kstat_default() {
        let kstat = Kstat::default();
        assert_eq!(kstat.dev, 0);
        assert_eq!(kstat.ino, 1);
        assert_eq!(kstat.nlink, 1);
        assert_eq!(kstat.mode, 0);
        assert_eq!(kstat.uid, 1);
        assert_eq!(kstat.gid, 1);
        assert_eq!(kstat.size, 0);
        assert_eq!(kstat.blksize, 4096);
        assert_eq!(kstat.blocks, 0);
        assert_eq!(kstat.atime, Duration::default());
        assert_eq!(kstat.mtime, Duration::default());
        assert_eq!(kstat.ctime, Duration::default());
    }

    #[def_test]
    fn test_kstat_to_stat() {
        let kstat = Kstat {
            dev: 42,
            ino: 100,
            nlink: 3,
            mode: 0o755,
            uid: 1000,
            gid: 1000,
            size: 4096,
            blksize: 512,
            blocks: 8,
            rdev: DeviceId::default(),
            atime: Duration::new(1000, 500_000_000),
            mtime: Duration::new(2000, 0),
            ctime: Duration::new(3000, 123_456_789),
        };

        let s: stat = kstat.into();
        assert_eq!(s.st_dev, 42);
        assert_eq!(s.st_ino, 100);
        assert_eq!(s.st_nlink, 3);
        assert_eq!(s.st_mode, 0o755);
        assert_eq!(s.st_uid, 1000);
        assert_eq!(s.st_gid, 1000);
        assert_eq!(s.st_size, 4096);
        assert_eq!(s.st_blksize, 512);
        assert_eq!(s.st_blocks, 8);
        assert_eq!(s.st_atime, 1000);
        assert_eq!(s.st_atime_nsec, 500_000_000);
        assert_eq!(s.st_mtime, 2000);
        assert_eq!(s.st_mtime_nsec, 0);
        assert_eq!(s.st_ctime, 3000);
        assert_eq!(s.st_ctime_nsec, 123_456_789);
    }

    #[def_test]
    fn test_kstat_to_statx() {
        let kstat = Kstat {
            dev: (5u64 << 32) | 10,
            ino: 200,
            nlink: 2,
            mode: 0o644,
            uid: 500,
            gid: 500,
            size: 8192,
            blksize: 4096,
            blocks: 16,
            rdev: DeviceId::default(),
            atime: Duration::new(100, 999_999_999),
            mtime: Duration::new(200, 0),
            ctime: Duration::new(300, 1),
        };

        let sx: statx = kstat.into();
        assert_eq!(sx.stx_ino, 200);
        assert_eq!(sx.stx_nlink, 2);
        assert_eq!(sx.stx_mode, 0o644);
        assert_eq!(sx.stx_uid, 500);
        assert_eq!(sx.stx_gid, 500);
        assert_eq!(sx.stx_size, 8192);
        assert_eq!(sx.stx_blksize, 4096);
        assert_eq!(sx.stx_blocks, 16);
        assert_eq!(sx.stx_dev_major, 5);
        assert_eq!(sx.stx_dev_minor, 10);
        assert_eq!(sx.stx_atime.tv_sec, 100);
        assert_eq!(sx.stx_atime.tv_nsec, 999_999_999);
        assert_eq!(sx.stx_mtime.tv_sec, 200);
        assert_eq!(sx.stx_mtime.tv_nsec, 0);
        assert_eq!(sx.stx_ctime.tv_sec, 300);
        assert_eq!(sx.stx_ctime.tv_nsec, 1);
    }

    #[def_test]
    fn test_kstat_default_to_stat_zeroed() {
        let s: stat = Kstat::default().into();
        assert_eq!(s.st_dev, 0);
        assert_eq!(s.st_size, 0);
        assert_eq!(s.st_blksize, 4096);
        assert_eq!(s.st_ino, 1);
        assert_eq!(s.st_nlink, 1);
    }
}

pub fn add_stdio(fd_table: &mut FlattenObjects<FileDescriptor, { FILE_LIMIT }>) -> KResult<()> {
    assert_eq!(fd_table.count(), 0);
    let cx = FS_CONTEXT.lock();
    let open = |options: &mut OpenOptions| {
        KResult::Ok(Arc::new(File::new(
            options.open(&cx, "/dev/console")?.into_file()?,
        )))
    };

    let tty_in = open(OpenOptions::new().read(true).write(false))?;
    let tty_out = open(OpenOptions::new().read(false).write(true))?;
    fd_table
        .add(FileDescriptor {
            inner: tty_in,
            cloexec: false,
        })
        .map_err(|_| KError::TooManyOpenFiles)?;
    fd_table
        .add(FileDescriptor {
            inner: tty_out.clone(),
            cloexec: false,
        })
        .map_err(|_| KError::TooManyOpenFiles)?;
    fd_table
        .add(FileDescriptor {
            inner: tty_out,
            cloexec: false,
        })
        .map_err(|_| KError::TooManyOpenFiles)?;

    Ok(())
}
