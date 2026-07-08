// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File descriptor entries stored in an [`FdTable`](crate::FdTable).

use alloc::sync::Arc;
use core::ffi::c_int;

use kerrno::KResult;
use kvfs::{Path, VfsFile};

/// A file descriptor entry in the file descriptor table.
#[derive(Clone)]
pub struct FileDescriptor {
    file: Arc<VfsFile>,
    cloexec: bool,
}

impl FileDescriptor {
    /// Creates a new descriptor entry.
    pub(crate) fn new(file: Arc<VfsFile>, cloexec: bool) -> Self {
        Self { file, cloexec }
    }

    /// Returns the underlying open file.
    pub fn file(&self) -> &Arc<VfsFile> {
        &self.file
    }

    /// Closes the descriptor's file reference.
    pub fn close(self) -> KResult {
        self.file.close_file()
    }

    /// Returns whether this descriptor is marked close-on-exec.
    pub(crate) fn cloexec(&self) -> bool {
        self.cloexec
    }

    /// Updates the close-on-exec bit for this descriptor.
    pub(crate) fn set_cloexec(&mut self, cloexec: bool) {
        self.cloexec = cloexec;
    }

    pub(crate) fn snapshot(&self, fd: c_int) -> FdSnapshot {
        FdSnapshot::new(fd, self.file.clone(), self.cloexec)
    }
}

/// Stable view of a descriptor table entry.
///
/// A snapshot owns a strong reference to the descriptor's open file and
/// copies descriptor flags while the fd table is locked. Callers can then drop
/// the table lock before following procfs magic links, opening files, or
/// loading exec images.
#[derive(Clone)]
pub struct FdSnapshot {
    fd: c_int,
    file: Arc<VfsFile>,
    cloexec: bool,
    open_flags: u32,
}

impl FdSnapshot {
    fn new(fd: c_int, file: Arc<VfsFile>, cloexec: bool) -> Self {
        let open_flags = file.flags();
        Self {
            fd,
            file,
            cloexec,
            open_flags,
        }
    }

    /// Returns the descriptor number that was snapshotted.
    pub fn fd(&self) -> c_int {
        self.fd
    }

    /// Returns the snapshotted open file.
    pub fn file(&self) -> &Arc<VfsFile> {
        &self.file
    }

    /// Returns whether the descriptor was marked close-on-exec.
    pub fn cloexec(&self) -> bool {
        self.cloexec
    }

    /// Returns object-level open flags captured with the snapshot.
    pub fn open_flags(&self) -> u32 {
        self.open_flags
    }

    /// Returns the VFS path referenced by this descriptor target.
    pub fn path(&self) -> &Path {
        self.file.path()
    }
}
