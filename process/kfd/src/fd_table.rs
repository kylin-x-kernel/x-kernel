// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File descriptor table ownership and operations.

use alloc::{sync::Arc, vec::Vec};
use core::ffi::c_int;

use flatten_objects::FlattenObjects;
use kerrno::{KError, KResult};
use ksync::RwLock;
use kvfs::VfsFile;

use crate::{FdSnapshot, FileDescriptor};

/// Process-local file descriptor table.
pub struct FdTable {
    entries: FlattenObjects<FileDescriptor, { krlimit::FILE_LIMIT }>,
}

impl Default for FdTable {
    fn default() -> Self {
        Self {
            entries: FlattenObjects::new(),
        }
    }
}

impl FdTable {
    /// Creates a shared descriptor table handle.
    pub fn new_shared() -> Arc<RwLock<Self>> {
        Arc::new(RwLock::new(Self::default()))
    }

    /// Clones a shared descriptor table handle with copied contents.
    pub fn clone_shared_from(source: &Arc<RwLock<Self>>) -> Arc<RwLock<Self>> {
        let source = source.read();
        let mut cloned = Self::default();
        cloned.clone_from(&source);
        Arc::new(RwLock::new(cloned))
    }

    /// Returns the number of occupied descriptors.
    pub fn count(&self) -> usize {
        self.entries.count()
    }

    /// Returns an iterator over allocated descriptor numbers.
    pub fn ids(&self) -> impl DoubleEndedIterator<Item = usize> + '_ {
        self.entries.ids()
    }

    /// Returns the descriptor entry at the given index.
    pub fn get(&self, fd: usize) -> Option<&FileDescriptor> {
        self.entries.get(fd)
    }

    /// Returns a mutable descriptor entry at the given index.
    pub(crate) fn get_mut(&mut self, fd: usize) -> Option<&mut FileDescriptor> {
        self.entries.get_mut(fd)
    }

    /// Inserts a descriptor entry at the first available slot.
    pub(crate) fn add(&mut self, descriptor: FileDescriptor) -> Result<usize, FileDescriptor> {
        self.entries.add(descriptor)
    }

    /// Inserts a descriptor entry at a fixed slot.
    pub(crate) fn add_at(
        &mut self,
        fd: usize,
        descriptor: FileDescriptor,
    ) -> Result<usize, FileDescriptor> {
        self.entries.add_at(fd, descriptor)
    }

    /// Removes the descriptor entry at the given index.
    pub(crate) fn remove(&mut self, fd: usize) -> Option<FileDescriptor> {
        self.entries.remove(fd)
    }

    /// Clones the table contents from another descriptor table.
    pub(crate) fn clone_from(&mut self, other: &Self) {
        self.entries.clone_from(&other.entries);
    }

    /// Returns the open file stored in the given descriptor.
    pub fn get_file(&self, fd: c_int) -> KResult<Arc<VfsFile>> {
        self.get(fd as usize)
            .map(|descriptor| descriptor.file().clone())
            .ok_or(KError::BadFileDescriptor)
    }

    /// Returns a stable snapshot of the descriptor entry.
    pub fn snapshot(&self, fd: c_int) -> KResult<FdSnapshot> {
        self.get(fd as usize)
            .map(|descriptor| descriptor.snapshot(fd))
            .ok_or(KError::BadFileDescriptor)
    }

    /// Adds an open file while enforcing the process soft limit.
    pub fn add_file(
        &mut self,
        max_nofile: u64,
        file: Arc<VfsFile>,
        cloexec: bool,
    ) -> KResult<c_int> {
        if self.count() as u64 >= max_nofile {
            return Err(KError::TooManyOpenFiles);
        }

        self.add(FileDescriptor::new(file, cloexec))
            .map(|fd| fd as c_int)
            .map_err(|_| KError::TooManyOpenFiles)
    }

    /// Inserts an open file without applying a resource-limit policy.
    pub fn insert_file(
        &mut self,
        file: Arc<VfsFile>,
        cloexec: bool,
    ) -> Result<usize, FileDescriptor> {
        self.add(FileDescriptor::new(file, cloexec))
    }

    /// Removes a descriptor from the table.
    pub fn file_close_fd_locked(&mut self, fd: c_int) -> KResult<FileDescriptor> {
        self.remove(fd as usize).ok_or(KError::BadFileDescriptor)
    }

    /// Returns the close-on-exec bit for the given descriptor.
    pub fn cloexec(&self, fd: c_int) -> KResult<bool> {
        Ok(self
            .get(fd as usize)
            .ok_or(KError::BadFileDescriptor)?
            .cloexec())
    }

    /// Updates the close-on-exec bit for the given descriptor.
    pub fn set_cloexec(&mut self, fd: c_int, cloexec: bool) -> KResult {
        self.get_mut(fd as usize)
            .ok_or(KError::BadFileDescriptor)?
            .set_cloexec(cloexec);
        Ok(())
    }

    /// Duplicates one descriptor into a fixed target slot.
    pub fn duplicate_to(
        &mut self,
        old_fd: c_int,
        new_fd: c_int,
        cloexec: bool,
    ) -> KResult<(c_int, Option<FileDescriptor>)> {
        if new_fd < 0 || new_fd as usize >= krlimit::FILE_LIMIT {
            return Err(KError::BadFileDescriptor);
        }

        if old_fd == new_fd {
            return self
                .get(old_fd as usize)
                .map(|_| (new_fd, None))
                .ok_or(KError::BadFileDescriptor);
        }

        let mut descriptor = self
            .get(old_fd as usize)
            .cloned()
            .ok_or(KError::BadFileDescriptor)?;
        descriptor.set_cloexec(cloexec);

        let new_fd_index = new_fd as usize;
        let replaced = self.remove(new_fd_index);
        self.add_at(new_fd_index, descriptor)
            .map(|_| (new_fd, replaced))
            .map_err(|_| KError::BadFileDescriptor)
    }

    /// Removes all descriptors in the given inclusive range.
    pub fn remove_range(&mut self, first_fd: c_int, last_fd: c_int) -> Vec<FileDescriptor> {
        let mut removed = Vec::new();
        let max_index = self.ids().next_back();
        if let Some(max_index) = max_index {
            for fd in first_fd..=last_fd.min(max_index as c_int) {
                if let Some(descriptor) = self.remove(fd as usize) {
                    removed.push(descriptor);
                }
            }
        }
        removed
    }

    /// Marks all descriptors in the given inclusive range close-on-exec.
    pub fn set_cloexec_range(&mut self, first_fd: c_int, last_fd: c_int) {
        let max_index = self.ids().next_back();
        if let Some(max_index) = max_index {
            for fd in first_fd..=last_fd.min(max_index as c_int) {
                if let Some(descriptor) = self.get_mut(fd as usize) {
                    descriptor.set_cloexec(true);
                }
            }
        }
    }

    /// Removes every descriptor currently marked close-on-exec.
    pub fn remove_cloexec_files(&mut self) -> Vec<FileDescriptor> {
        let cloexec_fds = self
            .ids()
            .filter(|fd| self.get(*fd).is_some_and(FileDescriptor::cloexec))
            .collect::<Vec<_>>();

        let mut removed = Vec::with_capacity(cloexec_fds.len());
        for fd in cloexec_fds {
            if let Some(descriptor) = self.remove(fd) {
                removed.push(descriptor);
            }
        }
        removed
    }

    /// Removes all descriptors when the table is not shared.
    pub fn remove_all_if_unshared(fd_table: &Arc<RwLock<Self>>) -> Vec<FileDescriptor> {
        if Arc::strong_count(fd_table) > 1 {
            return Vec::new();
        }

        let mut table = fd_table.write();
        let ids: Vec<usize> = table.ids().collect();
        let mut removed = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(descriptor) = table.remove(id) {
                removed.push(descriptor);
            }
        }
        removed
    }
}
