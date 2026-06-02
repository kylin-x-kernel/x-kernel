// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File descriptor table ownership and operations.

use alloc::{sync::Arc, vec::Vec};
use core::ffi::c_int;

use downcast_rs::DowncastSync;
use flatten_objects::FlattenObjects;
use kerrno::{KError, KResult};
use ksync::RwLock;

use crate::{FileDescriptor, FileLike};

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

    /// Returns the file-like object stored in the given descriptor.
    pub fn get_file_like(&self, fd: c_int) -> KResult<Arc<dyn FileLike>> {
        self.get(fd as usize)
            .map(|descriptor| descriptor.inner().clone())
            .ok_or(KError::BadFileDescriptor)
    }

    /// Returns the typed file-like object stored in the given descriptor.
    pub fn get_file_like_as<T>(&self, fd: c_int) -> KResult<Arc<T>>
    where
        T: FileLike + DowncastSync + 'static,
    {
        self.get_file_like(fd)?
            .downcast_arc()
            .map_err(|_| KError::InvalidInput)
    }

    /// Adds a file-like object while enforcing the process soft limit.
    pub fn add_file_like(
        &mut self,
        max_nofile: u64,
        file_like: Arc<dyn FileLike>,
        cloexec: bool,
    ) -> KResult<c_int> {
        if self.count() as u64 >= max_nofile {
            return Err(KError::TooManyOpenFiles);
        }

        self.add(FileDescriptor::new(file_like, cloexec))
            .map(|fd| fd as c_int)
            .map_err(|_| KError::TooManyOpenFiles)
    }

    /// Inserts a file-like object without applying a resource-limit policy.
    pub fn insert_file_like(
        &mut self,
        file_like: Arc<dyn FileLike>,
        cloexec: bool,
    ) -> Result<usize, FileDescriptor> {
        self.add(FileDescriptor::new(file_like, cloexec))
    }

    /// Closes a descriptor and removes it from the table.
    pub fn close_file_like(&mut self, fd: c_int) -> KResult {
        self.remove(fd as usize).ok_or(KError::BadFileDescriptor)?;
        Ok(())
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
    pub fn duplicate_to(&mut self, old_fd: c_int, new_fd: c_int, cloexec: bool) -> KResult<c_int> {
        let mut descriptor = self
            .get(old_fd as usize)
            .cloned()
            .ok_or(KError::BadFileDescriptor)?;
        descriptor.set_cloexec(cloexec);

        self.remove(new_fd as usize);
        self.add_at(new_fd as usize, descriptor)
            .map(|_| new_fd)
            .map_err(|_| KError::BadFileDescriptor)
    }

    /// Closes all descriptors in the given inclusive range.
    pub fn close_range(&mut self, first_fd: c_int, last_fd: c_int) {
        let max_index = self.ids().next_back();
        if let Some(max_index) = max_index {
            for fd in first_fd..=last_fd.min(max_index as c_int) {
                self.remove(fd as usize);
            }
        }
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

    /// Closes every descriptor currently marked close-on-exec.
    pub fn close_cloexec_files(&mut self) {
        let cloexec_fds = self
            .ids()
            .filter(|fd| self.get(*fd).is_some_and(FileDescriptor::cloexec))
            .collect::<Vec<_>>();

        for fd in cloexec_fds {
            self.remove(fd);
        }
    }

    /// Closes all descriptors when the table is not shared.
    pub fn close_all_if_unshared(fd_table: &Arc<RwLock<Self>>) {
        if Arc::strong_count(fd_table) > 1 {
            return;
        }

        let mut table = fd_table.write();
        let ids: Vec<usize> = table.ids().collect();
        let mut removed = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(descriptor) = table.remove(id) {
                removed.push(descriptor);
            }
        }
        drop(table);
        drop(removed);
    }
}
