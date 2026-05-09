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
    fn read(&self, _dst: &mut IoDst) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    fn write(&self, _src: &mut IoSrc) -> KResult<usize> {
        Err(KError::InvalidInput)
    }

    fn stat(&self) -> KResult<Kstat> {
        Ok(Kstat::default())
    }

    fn path(&self) -> Cow<'_, str>;

    fn ioctl(&self, _cmd: u32, _arg: usize) -> KResult<usize> {
        Err(KError::NotATty)
    }

    fn open_flags(&self) -> u32 {
        0
    }

    fn nonblocking(&self) -> bool {
        false
    }

    fn set_nonblocking(&self, _nonblocking: bool) -> KResult {
        Ok(())
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
