// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File-like trait surface shared by descriptor-backed objects.

use alloc::{borrow::Cow, sync::Arc};
use core::ffi::c_int;

use downcast_rs::{DowncastSync, impl_downcast};
use kerrno::{KError, KResult};
use kio::prelude::*;
use kpoll::Pollable;
use ksync::RwLock;
use kvfs::MmapMapper;

use crate::{FdTable, Kstat};

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
#[allow(dead_code)]
pub trait FileLike: Pollable + DowncastSync {
    /// Reads bytes from this object into `dst`.
    ///
    /// The default implementation reports that the object is not readable.
    fn read(&self, _dst: &mut IoDst) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    /// Writes bytes from `src` into this object.
    ///
    /// The default implementation reports that the object is not writable.
    fn write(&self, _src: &mut IoSrc) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    /// Returns metadata for this object.
    ///
    /// Implementations should override this when they can expose meaningful
    /// inode, mode, ownership, size, or timestamp data.
    fn stat(&self) -> KResult<Kstat> {
        Ok(Kstat::default())
    }

    /// Returns a display path for this object.
    fn path(&self) -> Cow<'_, str>;

    /// Handles an ioctl command for this object.
    ///
    /// The default implementation reports that the object is not a TTY.
    fn ioctl(&self, _cmd: u32, _arg: usize) -> KResult<usize> {
        Err(KError::NotATty)
    }

    /// Returns object-level open flags.
    ///
    /// Descriptor flags such as close-on-exec are stored in [`FileDescriptor`](crate::FileDescriptor),
    /// not in this value.
    fn open_flags(&self) -> u32 {
        0
    }

    /// Returns whether this object is in nonblocking mode.
    fn nonblocking(&self) -> bool {
        false
    }

    /// Updates this object's nonblocking mode.
    ///
    /// The default implementation accepts the request and leaves the object
    /// unchanged, for objects without a separate nonblocking state.
    fn set_nonblocking(&self, _nonblocking: bool) -> KResult {
        Ok(())
    }

    /// Handle mmap for this file via the provided mapper.
    /// Default returns `ENODEV` (mmap not supported).
    fn mmap(&self, _mapper: &mut dyn MmapMapper) -> KResult<()> {
        Err(KError::NoSuchDevice)
    }

    /// Returns a typed descriptor entry from a specific descriptor table.
    ///
    /// This is a low-level helper for resource-owner code and cross-process paths.
    /// Current-context callers should go through `ProcessResources` instead of
    /// passing raw descriptor-table handles around.
    fn from_fd(fd_table: &RwLock<FdTable>, fd: c_int) -> KResult<Arc<Self>>
    where
        Self: Sized + 'static,
    {
        fd_table
            .read()
            .get_file_like(fd)?
            .downcast_arc()
            .map_err(|_| KError::InvalidInput)
    }

    /// Installs `self` into a specific descriptor table.
    ///
    /// This is a low-level helper for resource-owner code and explicit
    /// cross-process installation paths. Current-context callers should use
    /// `ProcessResources::add_file_like`.
    fn add_to_fd_table(
        self,
        fd_table: &RwLock<FdTable>,
        max_nofile: u64,
        cloexec: bool,
    ) -> KResult<c_int>
    where
        Self: Sized + 'static,
    {
        fd_table
            .write()
            .add_file_like(max_nofile, Arc::new(self), cloexec)
    }
}

impl_downcast!(sync FileLike);
