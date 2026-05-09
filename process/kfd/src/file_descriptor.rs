// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File descriptor entries stored in an [`FdTable`](crate::FdTable).

use alloc::sync::Arc;

use crate::FileLike;

/// A file descriptor entry in the file descriptor table.
#[derive(Clone)]
pub struct FileDescriptor {
    inner: Arc<dyn FileLike>,
    cloexec: bool,
}

impl FileDescriptor {
    /// Creates a new descriptor entry.
    pub fn new(inner: Arc<dyn FileLike>, cloexec: bool) -> Self {
        Self { inner, cloexec }
    }

    /// Returns the underlying file-like object.
    pub fn inner(&self) -> &Arc<dyn FileLike> {
        &self.inner
    }

    /// Returns whether this descriptor is marked close-on-exec.
    pub fn cloexec(&self) -> bool {
        self.cloexec
    }

    /// Updates the close-on-exec bit for this descriptor.
    pub fn set_cloexec(&mut self, cloexec: bool) {
        self.cloexec = cloexec;
    }
}
