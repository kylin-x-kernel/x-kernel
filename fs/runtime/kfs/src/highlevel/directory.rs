// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Directory type with `FileLike` support.

use alloc::{borrow::Cow, string::ToString, sync::Arc};
use core::{ffi::c_int, task::Context};

use kerrno::{KError, KResult};
use kfd::{FdTable, FileLike, IoDst, IoSrc, Kstat};
use kpoll::{IoEvents, Pollable};
use ksync::{Mutex, RwLock};
use kvfs::Location;

use super::path_for;

/// Directory wrapper that provides directory operations through the `FileLike` trait.
pub struct Directory {
    inner: Location,
    pub offset: Mutex<u64>,
}

impl Directory {
    pub fn new(inner: Location) -> Self {
        Self {
            inner,
            offset: Mutex::new(0),
        }
    }

    pub fn inner(&self) -> &Location {
        &self.inner
    }
}

impl FileLike for Directory {
    fn read(&self, _dst: &mut IoDst) -> KResult<usize> {
        Err(KError::BadFileDescriptor)
    }

    fn write(&self, _src: &mut IoSrc) -> KResult<usize> {
        Err(KError::BadFileDescriptor)
    }

    fn stat(&self) -> KResult<Kstat> {
        Ok(Kstat::from(self.inner.metadata()?))
    }

    fn path(&self) -> Cow<'_, str> {
        path_for(&self.inner)
    }

    fn from_fd(fd_table: &RwLock<FdTable>, fd: c_int) -> KResult<Arc<Self>> {
        fd_table
            .read()
            .get_file_like(fd)?
            .downcast_arc()
            .map_err(|_| KError::NotADirectory)
    }
}

impl Pollable for Directory {
    fn poll(&self) -> IoEvents {
        IoEvents::IN | IoEvents::OUT
    }

    fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
}
