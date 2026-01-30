// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Filesystem-backed file and directory wrappers.

use alloc::{borrow::Cow, string::ToString, sync::Arc};
use core::{
    ffi::c_int,
    hint::likely,
    sync::atomic::{AtomicBool, Ordering},
    task::Context,
};

use fs_ng_vfs::{Location, Metadata, NodeFlags};
use kerrno::{KError, KResult};
use kfs::{FS_CONTEXT, FsContext};
use kpoll::{IoEvents, Pollable};
use ksync::Mutex;
use ktask::future::{block_on, poll_io};
use linux_raw_sys::general::{AT_EMPTY_PATH, AT_FDCWD, AT_SYMLINK_NOFOLLOW};

use super::{FileLike, Kstat, get_file_like};
use crate::file::{IoDst, IoSrc};

/// Executes a function with the file system context for the given directory file descriptor.
///
/// If `dirfd` is `AT_FDCWD`, uses the current directory context.
/// Otherwise, resolves the directory from the given file descriptor and uses it as the base.
pub fn with_fs<R>(dirfd: c_int, f: impl FnOnce(&mut FsContext) -> KResult<R>) -> KResult<R> {
    let mut fs = FS_CONTEXT.lock();
    if dirfd == AT_FDCWD {
        f(&mut fs)
    } else {
        let dir = Directory::from_fd(dirfd)?.inner.clone();
        f(&mut fs.with_current_dir(dir)?)
    }
}

/// Result of resolving a path at a given directory.
///
/// Can be either a regular file resolved to a filesystem location, or a special file-like object.
pub enum ResolveAtResult {
    File(Location),
    Other(Arc<dyn FileLike>),
}

impl ResolveAtResult {
    /// Extracts the file location if this is a regular file, otherwise returns `None`.
    pub fn into_file(self) -> Option<Location> {
        match self {
            Self::File(file) => Some(file),
            Self::Other(_) => None,
        }
    }

    /// Gets file statistics for this resolved path.
    pub fn stat(&self) -> KResult<Kstat> {
        match self {
            Self::File(file) => file.metadata().map(|it| metadata_to_kstat(&it)),
            Self::Other(file_like) => file_like.stat(),
        }
    }
}

/// Resolves a path relative to the given directory file descriptor.
///
/// # Arguments
/// - `dirfd`: The directory file descriptor, or `AT_FDCWD` for current directory
/// - `path`: The path to resolve. If `None` or empty, requires `AT_EMPTY_PATH` flag
/// - `flags`: Resolution flags (e.g., `AT_SYMLINK_NOFOLLOW`)
pub fn resolve_at(dirfd: c_int, path: Option<&str>, flags: u32) -> KResult<ResolveAtResult> {
    match path {
        Some("") | None => {
            if flags & AT_EMPTY_PATH == 0 {
                return Err(KError::NotFound);
            }
            let file_like = get_file_like(dirfd)?;
            let f = file_like.clone();
            Ok(if let Some(file) = f.downcast_ref::<File>() {
                ResolveAtResult::File(file.inner().backend()?.location().clone())
            } else if let Some(dir) = f.downcast_ref::<Directory>() {
                ResolveAtResult::File(dir.inner().clone())
            } else {
                ResolveAtResult::Other(file_like)
            })
        }
        Some(path) => with_fs(dirfd, |fs| {
            if flags & AT_SYMLINK_NOFOLLOW != 0 {
                fs.resolve_no_follow(path)
            } else {
                fs.resolve(path)
            }
            .map(ResolveAtResult::File)
        }),
    }
}

/// Converts filesystem metadata to kernel stat structure.
pub fn metadata_to_kstat(metadata: &Metadata) -> Kstat {
    let ty = metadata.node_type as u8;
    let perm = metadata.mode.bits() as u32;
    let mode = ((ty as u32) << 12) | perm;
    Kstat {
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

/// File wrapper for `kfs::fops::File`.
///
/// Manages blocking/non-blocking I/O and provides file operations through the `FileLike` trait.
pub struct File {
    inner: kfs::File,
    /// Non-blocking flag for this file descriptor
    nonblock: AtomicBool,
}

impl File {
    /// Creates a new file wrapper from the underlying kernel file.
    pub fn new(inner: kfs::File) -> Self {
        Self {
            inner,
            nonblock: AtomicBool::new(false),
        }
    }

    /// Returns a reference to the underlying kernel file.
    pub fn inner(&self) -> &kfs::File {
        &self.inner
    }

    /// Checks if this file is configured for blocking I/O.
    fn is_blocking(&self) -> bool {
        self.inner.location().flags().contains(NodeFlags::BLOCKING)
    }
}

/// Gets the absolute path of a location, or `<error>` if unavailable.
fn path_for(loc: &Location) -> Cow<'static, str> {
    loc.absolute_path()
        .map_or_else(|_| "<error>".into(), |f| Cow::Owned(f.to_string()))
}

impl FileLike for File {
    /// Reads from the file, using non-blocking I/O when needed.
    fn read(&self, dst: &mut IoDst) -> KResult<usize> {
        let inner = self.inner();
        if likely(self.is_blocking()) {
            inner.read(dst)
        } else {
            block_on(poll_io(self, IoEvents::IN, self.nonblocking(), || {
                inner.read(&mut *dst)
            }))
        }
    }

    /// Writes to the file, using non-blocking I/O when needed.
    fn write(&self, src: &mut IoSrc) -> KResult<usize> {
        let inner = self.inner();
        if likely(self.is_blocking()) {
            inner.write(src)
        } else {
            block_on(poll_io(self, IoEvents::OUT, self.nonblocking(), || {
                inner.write(&mut *src)
            }))
        }
    }

    /// Gets file statistics.
    fn stat(&self) -> KResult<Kstat> {
        Ok(metadata_to_kstat(&self.inner().location().metadata()?))
    }

    /// Performs I/O control operation.
    fn ioctl(&self, cmd: u32, arg: usize) -> KResult<usize> {
        self.inner().backend()?.location().ioctl(cmd, arg)
    }

    /// Sets or clears the non-blocking flag.
    fn set_nonblocking(&self, flag: bool) -> KResult {
        self.nonblock.store(flag, Ordering::Release);
        Ok(())
    }

    /// Returns whether non-blocking mode is enabled.
    fn nonblocking(&self) -> bool {
        self.nonblock.load(Ordering::Acquire)
    }

    /// Returns the absolute path of the file.
    fn path(&self) -> Cow<'_, str> {
        path_for(self.inner.location())
    }

    /// Converts a file descriptor to a file reference.
    fn from_fd(fd: c_int) -> KResult<Arc<Self>>
    where
        Self: Sized + 'static,
    {
        get_file_like(fd)?.downcast_arc().map_err(|any| {
            if any.is::<Directory>() {
                KError::IsADirectory
            } else {
                KError::BrokenPipe
            }
        })
    }
}
impl Pollable for File {
    /// Polls for available I/O events on this file.
    fn poll(&self) -> IoEvents {
        self.inner().location().poll()
    }

    /// Registers the file for polling with the given context and events.
    fn register(&self, context: &mut Context<'_>, events: IoEvents) {
        self.inner().location().register(context, events);
    }
}

/// Directory wrapper for `kfs::fops::Directory`.
///
/// Manages directory traversal and provides directory operations through the `FileLike` trait.
pub struct Directory {
    inner: Location,
    /// Current offset for directory iteration
    pub offset: Mutex<u64>,
}

impl Directory {
    /// Creates a new directory wrapper from the given location.
    pub fn new(inner: Location) -> Self {
        Self {
            inner,
            offset: Mutex::new(0),
        }
    }

    /// Get the inner node of the directory.
    pub fn inner(&self) -> &Location {
        &self.inner
    }
}

impl FileLike for Directory {
    /// Read is not supported on directories.
    fn read(&self, _dst: &mut IoDst) -> KResult<usize> {
        Err(KError::BadFileDescriptor)
    }

    /// Write is not supported on directories.
    fn write(&self, _src: &mut IoSrc) -> KResult<usize> {
        Err(KError::BadFileDescriptor)
    }

    /// Gets directory statistics.
    fn stat(&self) -> KResult<Kstat> {
        Ok(metadata_to_kstat(&self.inner.metadata()?))
    }

    /// Returns the absolute path of the directory.
    fn path(&self) -> Cow<'_, str> {
        path_for(&self.inner)
    }

    /// Converts a file descriptor to a directory reference.
    fn from_fd(fd: c_int) -> KResult<Arc<Self>> {
        get_file_like(fd)?
            .downcast_arc()
            .map_err(|_| KError::NotADirectory)
    }
}
impl Pollable for Directory {
    /// Directories are always ready for reading and writing metadata.
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    /// Directories do not support polling registration.
    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}
