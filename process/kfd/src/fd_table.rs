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
    /// Returns the POSIX lock owner for this table at its stable shared address.
    /// Shared tables retain the identity; a copied table obtains a new identity.
    pub fn lock_owner(&self) -> kvfs::FileLockOwner {
        kvfs::FileLockOwner::posix(self)
    }

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
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` if `fd` is negative, outside the table,
    /// or does not identify an occupied slot.
    pub fn get_file(&self, fd: c_int) -> KResult<Arc<VfsFile>> {
        self.get(fd as usize)
            .map(|descriptor| descriptor.file().clone())
            .ok_or(KError::BadFileDescriptor)
    }

    /// Returns a stable snapshot of the descriptor entry.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` if `fd` is negative, outside the table,
    /// or does not identify an occupied slot.
    pub fn snapshot(&self, fd: c_int) -> KResult<FdSnapshot> {
        self.get(fd as usize)
            .map(|descriptor| descriptor.snapshot(fd))
            .ok_or(KError::BadFileDescriptor)
    }

    /// Adds an open file while enforcing the process soft limit.
    ///
    /// # Errors
    ///
    /// Returns `KError::TooManyOpenFiles` if no free descriptor number is below
    /// both `max_nofile` and physical capacity. Existing high descriptors do not
    /// prevent reuse of a low hole after the soft limit is reduced.
    pub fn add_file(
        &mut self,
        max_nofile: u64,
        file: Arc<VfsFile>,
        cloexec: bool,
    ) -> KResult<c_int> {
        self.allocate_descriptor(0, max_nofile, FileDescriptor::new(file, cloexec))
    }

    /// Duplicates into the lowest free descriptor at or above `minimum`.
    ///
    /// The new descriptor must be below `max_nofile` and physical table capacity.
    /// Existing descriptors above a lowered soft limit remain valid sources and
    /// do not prevent allocation into a free slot below that limit. The caller
    /// holds the table write lock across source lookup and insertion.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` for an absent source, checked first;
    /// `KError::InvalidInput` for a negative minimum or one at/above `max_nofile`;
    /// and `KError::TooManyOpenFiles` when the permitted range has no free slot.
    /// Errors leave the table unchanged.
    pub fn duplicate_from(
        &mut self,
        old_fd: c_int,
        minimum: c_int,
        max_nofile: u64,
        cloexec: bool,
    ) -> KResult<c_int> {
        let source = self.get(old_fd as usize).ok_or(KError::BadFileDescriptor)?;
        if minimum < 0 || minimum as u64 >= max_nofile {
            return Err(KError::InvalidInput);
        }

        let descriptor = FileDescriptor::new(source.file().clone(), cloexec);
        self.allocate_descriptor(minimum as usize, max_nofile, descriptor)
    }

    /// Shared allocation for ordinary insertion and minimum-based duplication.
    /// Command-specific validation happens before this policy-neutral search.
    fn allocate_descriptor(
        &mut self,
        minimum: usize,
        max_nofile: u64,
        descriptor: FileDescriptor,
    ) -> KResult<c_int> {
        // Search is bounded by the fixed table capacity, even for RLIM_INFINITY.
        let end = max_nofile.min(krlimit::FILE_LIMIT as u64) as usize;
        self.entries
            .add_in_range(minimum, end, descriptor)
            .map(|fd| fd as c_int)
            .map_err(|_| KError::TooManyOpenFiles)
    }

    /// Inserts an open file without applying a resource-limit policy.
    ///
    /// # Errors
    ///
    /// Returns the uninserted `FileDescriptor` when the table is full; ownership is
    /// returned to the caller rather than discarding the open-file reference.
    pub fn insert_file(
        &mut self,
        file: Arc<VfsFile>,
        cloexec: bool,
    ) -> Result<usize, FileDescriptor> {
        self.add(FileDescriptor::new(file, cloexec))
    }

    /// Removes a descriptor from the table.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` if `fd` is negative, outside the table,
    /// or does not identify an occupied slot.
    pub fn file_close_fd_locked(&mut self, fd: c_int) -> KResult<FileDescriptor> {
        self.remove(fd as usize).ok_or(KError::BadFileDescriptor)
    }

    /// Returns the close-on-exec bit for the given descriptor.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` if `fd` is negative, outside the table,
    /// or does not identify an occupied slot.
    pub fn cloexec(&self, fd: c_int) -> KResult<bool> {
        Ok(self
            .get(fd as usize)
            .ok_or(KError::BadFileDescriptor)?
            .cloexec())
    }

    /// Updates the close-on-exec bit for the given descriptor.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` if `fd` is negative, outside the table,
    /// or does not identify an occupied slot.
    pub fn set_cloexec(&mut self, fd: c_int, cloexec: bool) -> KResult {
        self.get_mut(fd as usize)
            .ok_or(KError::BadFileDescriptor)?
            .set_cloexec(cloexec);
        Ok(())
    }

    /// Duplicates one descriptor into a fixed target slot.
    ///
    /// Returns the replaced descriptor, if any, for the caller to close after
    /// releasing the table lock. Equal descriptors leave flags unchanged. This
    /// method checks physical capacity, not the process soft limit.
    ///
    /// # Errors
    ///
    /// Returns `KError::BadFileDescriptor` for a negative/out-of-capacity target, an
    /// absent source, or failure to insert at the target slot.
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
}

impl Drop for FdTable {
    fn drop(&mut self) {
        // Final owner teardown cannot return close errors. Keep closing so one
        // failed flush does not retain the remaining descriptors.
        loop {
            let next_fd = self.ids().next();
            let Some(fd) = next_fd else {
                break;
            };
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: Exclusive descriptor-table \
                          teardown removes the ID just obtained from this same table"
            )]
            let descriptor = self.remove(fd).expect("descriptor id must remain present");
            // Teardown has no error channel and must close the remaining descriptors.
            let _ = descriptor.close(self.lock_owner());
        }
    }
}
