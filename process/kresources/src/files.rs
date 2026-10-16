// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::sync::Arc;
use core::{any::Any, ffi::c_int};

use kerrno::{KError, KResult};
use kfd::{FdSnapshot, FdTable, FdTableOwner, FdTableRef, FileDescriptor};
use ksync::RwLock;
use kvfs::VfsFile;

/// One task's detachable binding to a shared descriptor table.
///
/// Cloning a task creates a new binding, optionally sharing the table. Unshare
/// replaces only this binding. Table selection and descriptor mutation are
/// serialized; VFS close callbacks run after both locks have been released.
pub struct TaskFiles {
    /// The task's binding to its descriptor table.
    ///
    /// Only bindings retain the owner; temporary read-only references
    /// ([`FdTableRef`]) neither count as sharing nor delay descriptor teardown.
    fd_table: RwLock<Option<Arc<FdTableOwner>>>,
}

impl TaskFiles {
    /// Attaches a table owner to a new task; the binding itself is never shared.
    pub fn new(owner: Arc<FdTableOwner>) -> Self {
        Self {
            fd_table: RwLock::new(Some(owner)),
        }
    }

    /// Creates an empty private binding with a fresh table.
    ///
    /// Convenience for process and thread construction, which do not otherwise
    /// need the `kfd` owner type in scope.
    pub fn new_shared() -> Self {
        Self::new(FdTableOwner::new_shared())
    }

    /// Prepares a child's table binding, following `CLONE_FILES`.
    ///
    /// # Errors
    /// Returns `NoSuchProcess` after the task has detached its files.
    pub fn fork(&self, is_shared: bool) -> KResult<Self> {
        let binding = self.fd_table.read();
        let owner = binding.as_ref().ok_or(KError::NoSuchProcess)?;
        Ok(Self::new(if is_shared {
            owner.clone()
        } else {
            FdTableOwner::copy_from(owner)
        }))
    }

    /// Returns temporary read-only access to the attached table.
    ///
    /// This pins storage and lock-owner identity, not descriptor ownership. The
    /// last binding may drain its descriptors while this reference exists.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] after the task has released its files
    /// owner.
    pub fn fd_table(&self) -> KResult<FdTableRef> {
        self.fd_table
            .read()
            .as_ref()
            .map(|owner| owner.reference())
            .ok_or(KError::NoSuchProcess)
    }

    /// Accesses the current table while preventing replacement or detachment.
    ///
    /// The callback may lock the table, but must not replace the binding or
    /// wait for file locks: another task may need this binding to release them.
    ///
    /// # Errors
    /// Returns `NoSuchProcess` after detachment, or the callback's error.
    pub fn with_fd_table<R>(
        &self,
        access_fn: impl FnOnce(&RwLock<FdTable>) -> KResult<R>,
    ) -> KResult<R> {
        let binding = self.fd_table.read();
        let owner = binding.as_ref().ok_or(KError::NoSuchProcess)?;
        access_fn(owner.table())
    }

    fn close_descriptors(
        descriptors: impl IntoIterator<Item = FileDescriptor>,
        owner: kvfs::FileLockOwner,
    ) {
        for descriptor in descriptors {
            // Teardown must close every descriptor; close errors have no receiver.
            let _ = descriptor.close(owner);
        }
    }

    /// Returns the open file stored in the given descriptor.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment or
    /// `KError::BadFileDescriptor` if the descriptor is absent.
    pub fn get_file(&self, fd: c_int) -> kerrno::KResult<Arc<VfsFile>> {
        self.with_fd_table(|fd_table| fd_table.read().get_file(fd))
    }

    /// Returns a stable snapshot of the descriptor entry.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment or
    /// `KError::BadFileDescriptor` if the descriptor is absent.
    pub fn snapshot_fd(&self, fd: c_int) -> kerrno::KResult<FdSnapshot> {
        self.with_fd_table(|fd_table| fd_table.read().snapshot(fd))
    }

    /// Returns typed file-private data attached to the descriptor's open file.
    ///
    /// # Errors
    ///
    /// Propagates [`Self::get_file`] errors and returns `KError::InvalidInput`
    /// when the file has no private data of type `T`.
    pub fn get_file_private<T>(&self, fd: c_int) -> kerrno::KResult<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        self.get_file(fd)?
            .private_data_get::<T>()
            .ok_or(KError::InvalidInput)
    }

    /// Adds an open file to the current descriptor table.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment, or
    /// `KError::TooManyOpenFiles` if no free descriptor number is below the soft
    /// limit and physical table capacity.
    pub fn add_file(
        &self,
        max_nofile: u64,
        file: Arc<VfsFile>,
        cloexec: bool,
    ) -> kerrno::KResult<c_int> {
        self.with_fd_table(|fd_table| fd_table.write().add_file(max_nofile, file, cloexec))
    }

    /// Duplicates a descriptor into a newly allocated slot.
    ///
    /// # Errors
    ///
    /// Propagates source lookup errors from [`Self::get_file`] and insertion errors
    /// from [`Self::add_file`]. The new slot shares the same open file.
    pub fn duplicate_file(
        &self,
        max_nofile: u64,
        fd: c_int,
        cloexec: bool,
    ) -> kerrno::KResult<c_int> {
        self.with_fd_table(|table| {
            let mut table = table.write();
            let file = table.get_file(fd)?;
            table.add_file(max_nofile, file, cloexec)
        })
    }

    /// Duplicates a descriptor into the lowest free slot at/above `minimum`.
    ///
    /// The caller supplies the current process's soft limit before entering
    /// the binding/table locks. One table write guard covers source lookup and
    /// allocation; the binding read guard excludes detachment or replacement.
    ///
    /// # Errors
    /// Returns `NoSuchProcess` after files detachment. Otherwise propagates
    /// source, minimum, and exhaustion errors from [`FdTable::duplicate_from`].
    pub fn duplicate_file_from(
        &self,
        max_nofile: u64,
        fd: c_int,
        minimum: c_int,
        cloexec: bool,
    ) -> KResult<c_int> {
        self.with_fd_table(|table| {
            table
                .write()
                .duplicate_from(fd, minimum, max_nofile, cloexec)
        })
    }

    /// Duplicates a descriptor into a fixed slot.
    ///
    /// The table capacity is checked, but this method does not apply `RLIMIT_NOFILE`.
    /// A replaced descriptor is closed after unlocking; its close error is ignored.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment. Propagates
    /// `KError::BadFileDescriptor` from [`FdTable::duplicate_to`] for an absent source
    /// or invalid target.
    pub fn duplicate_file_to(
        &self,
        old_fd: c_int,
        new_fd: c_int,
        cloexec: bool,
    ) -> kerrno::KResult<c_int> {
        let binding = self.fd_table.read();
        let files = binding.as_ref().ok_or(KError::NoSuchProcess)?;
        // Pin the POSIX identity through close, without extending binding ownership.
        let _table = files.reference();
        let (fd, replaced, owner) = {
            let mut table = files.table().write();
            let (fd, replaced) = table.duplicate_to(old_fd, new_fd, cloexec)?;
            (fd, replaced, table.lock_owner())
        };
        drop(binding);
        Self::close_descriptors(replaced, owner);
        Ok(fd)
    }

    /// Closes the given file descriptor.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment or
    /// `KError::BadFileDescriptor` for an absent entry. Propagates the removed
    /// descriptor's VFS close/flush error; the slot stays removed even on that error.
    pub fn close_file(&self, fd: c_int) -> kerrno::KResult {
        let binding = self.fd_table.read();
        let files = binding.as_ref().ok_or(KError::NoSuchProcess)?;
        // Pin the POSIX identity through close, without extending binding ownership.
        let _table = files.reference();
        let (descriptor, owner) = {
            let mut table = files.table().write();
            (table.file_close_fd_locked(fd)?, table.lock_owner())
        };
        drop(binding);
        descriptor.close(owner)
    }

    /// Returns whether the given descriptor is marked close-on-exec.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment or
    /// `KError::BadFileDescriptor` if the descriptor is absent.
    pub fn cloexec(&self, fd: c_int) -> kerrno::KResult<bool> {
        self.with_fd_table(|fd_table| fd_table.read().cloexec(fd))
    }

    /// Updates the close-on-exec bit for the given descriptor.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment or
    /// `KError::BadFileDescriptor` if the descriptor is absent.
    pub fn set_cloexec(&self, fd: c_int, cloexec: bool) -> kerrno::KResult {
        self.with_fd_table(|fd_table| fd_table.write().set_cloexec(fd, cloexec))
    }

    /// Closes all descriptors in the given inclusive range.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment. Missing slots are
    /// ignored; batch close operations ignore individual descriptor close errors.
    pub fn close_range(&self, first_fd: c_int, last_fd: c_int) -> KResult<()> {
        let binding = self.fd_table.read();
        let files = binding.as_ref().ok_or(KError::NoSuchProcess)?;
        // Pin the POSIX identity through close, without extending binding ownership.
        let _table = files.reference();
        let (descriptors, owner) = {
            let mut table = files.table().write();
            (table.remove_range(first_fd, last_fd), table.lock_owner())
        };
        drop(binding);
        Self::close_descriptors(descriptors, owner);
        Ok(())
    }

    /// Marks all descriptors in the given inclusive range close-on-exec.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment. Missing slots are
    /// ignored; batch close operations ignore individual descriptor close errors.
    pub fn set_cloexec_range(&self, first_fd: c_int, last_fd: c_int) -> KResult<()> {
        self.with_fd_table(|fd_table| {
            fd_table.write().set_cloexec_range(first_fd, last_fd);
            Ok(())
        })
    }

    /// Closes all descriptors marked close-on-exec.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment. Missing slots are
    /// ignored; batch close operations ignore individual descriptor close errors.
    pub fn close_cloexec_files(&self) -> KResult<()> {
        let binding = self.fd_table.read();
        let files = binding.as_ref().ok_or(KError::NoSuchProcess)?;
        // Pin the POSIX identity through close, without extending binding ownership.
        let _table = files.reference();
        let (descriptors, owner) = {
            let mut table = files.table().write();
            (table.remove_cloexec_files(), table.lock_owner())
        };
        drop(binding);
        Self::close_descriptors(descriptors, owner);
        Ok(())
    }

    /// Releases this task's file descriptor table owner.
    ///
    /// The last binding closes its descriptors even when temporary table
    /// references remain; cleanup runs after releasing the binding lock.
    pub fn exit_files(&self) {
        let fd_table = self.fd_table.write().take();
        drop(fd_table);
    }

    /// Replaces a shared fd table with a private clone.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] after the files owner is released.
    ///
    /// # Panics
    ///
    /// Panics if the owner disappears after its presence was checked under the same
    /// write lock; this would indicate a broken internal ownership invariant.
    pub fn unshare_fd_table(&self) -> KResult<()> {
        let old_table = {
            let mut owner = self.fd_table.write();
            let table = owner.as_ref().ok_or(KError::NoSuchProcess)?;
            if Arc::strong_count(table) == 1 {
                return Ok(());
            }

            let new_table = FdTableOwner::copy_from(table);
            #[expect(
                clippy::expect_used,
                reason = "CG-021-no-panic-on-reachable-failure: The fd-table owner was checked \
                          while holding the same exclusive write lock"
            )]
            owner
                .replace(new_table)
                .expect("fd table owner was checked")
        };
        drop(old_table);
        Ok(())
    }
}
