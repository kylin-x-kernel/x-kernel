// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-owned limits and detachable descriptor-table references.
//!
//! Start with [`ProcessResources::new`]. Limits remain available after
//! [`ProcessResources::exit_files`]; the detached files owner cannot be revived.
//! These APIs require initialized kernel allocation and sleepable locks.
//!
//! # Example
//!
//! ```no_run
//! use kresources::ProcessResources;
//! let resources = ProcessResources::new(512 * 1024);
//! assert_eq!(resources.max_nofile(), 1024);
//! resources.exit_files();
//! assert!(resources.fd_table().is_err());
//! ```

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::{any::Any, ffi::c_int};

use kerrno::{KError, KResult};
use kfd::{FdSnapshot, FdTable, FdTableOwner, FdTableRef, FileDescriptor};
use krlimit::{Rlimit, Rlimits};
use ksync::RwLock;
use kvfs::VfsFile;
use linux_raw_sys::general::{RLIM_NLIMITS, RLIMIT_NOFILE};

/// Process-owned resource state.
///
/// The resource set owns limits and the detachable file descriptor table used
/// by one process runtime.
pub struct ProcessResources {
    /// Per-process resource limits.
    pub rlimits: RwLock<Rlimits>,
    /// The process-owned file descriptor table handle.
    fd_table: RwLock<Option<Arc<FdTableOwner>>>,
}

impl ProcessResources {
    /// Creates a new process resource set with default limits.
    ///
    /// `user_stack_size` is the configured user stack capacity in bytes,
    /// passed to [`Rlimits::new`]. The attached descriptor table starts empty.
    pub fn new(user_stack_size: usize) -> Arc<Self> {
        Arc::new(Self {
            rlimits: RwLock::new(Rlimits::new(user_stack_size)),
            fd_table: RwLock::new(Some(FdTableOwner::new_shared())),
        })
    }

    /// Returns the current RLIMIT_NOFILE soft cap.
    pub fn max_nofile(&self) -> u64 {
        self.rlimits.read()[RLIMIT_NOFILE].current
    }

    /// Returns the current limit for a specific resource.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` when `resource >= RLIM_NLIMITS`.
    pub fn rlimit(&self, resource: u32) -> KResult<Rlimit> {
        if resource >= RLIM_NLIMITS {
            return Err(KError::InvalidInput);
        }

        Ok(self.rlimits.read()[resource])
    }

    /// Updates the limit for a specific resource.
    ///
    /// Use this setter when the replaced pair is not needed. Call
    /// [`Self::update_rlimit_and_get_old`] to also return the pair replaced.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for an out-of-range resource or soft > hard.
    /// Returns `KError::OperationNotPermitted` for any hard-limit increase.
    /// The old pair is unchanged on failure; capability-based increases are not supported.
    pub fn set_rlimit(&self, resource: u32, new_limit: Rlimit) -> KResult {
        self.update_rlimit_and_get_old(resource, Some(new_limit))?;
        Ok(())
    }

    /// Returns the old limit and optionally replaces it in one atomic operation.
    ///
    /// `None` returns a checked snapshot through [`Self::rlimit`] under a limits
    /// read lock without changing the pair. `Some(new_limit)` returns the exact
    /// pair replaced after checking the update against the current hard limit.
    /// For an update, validation, the snapshot and replacement share one limits
    /// write lock; a failed update returns no snapshot and changes no state.
    ///
    /// Requires sleepable task context. This method does not access user memory;
    /// callers must copy inputs before calling and publish outputs after it returns.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for an out-of-range resource or soft > hard.
    /// Returns `KError::OperationNotPermitted` for any hard-limit increase;
    /// capability-based increases are not supported.
    pub fn update_rlimit_and_get_old(
        &self,
        resource: u32,
        new_limit: Option<Rlimit>,
    ) -> KResult<Rlimit> {
        let Some(new_limit) = new_limit else {
            return self.rlimit(resource);
        };
        if resource >= RLIM_NLIMITS {
            return Err(KError::InvalidInput);
        }

        let mut limits = self.rlimits.write();
        let limit = &mut limits[resource];
        let old_limit = *limit;
        if new_limit.current > new_limit.max {
            return Err(KError::InvalidInput);
        }
        if new_limit.max > old_limit.max {
            // Raising the hard limit requires CAP_SYS_RESOURCE.
            // Return EPERM until proper credential checks are in place.
            return Err(KError::OperationNotPermitted);
        }
        *limit = new_limit;

        Ok(old_limit)
    }

    /// Returns temporary read-only access to the attached file descriptor table.
    ///
    /// This pins storage and lock-owner identity, not descriptor ownership.
    /// The last binding may drain its descriptors while this reference exists.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] after the process has released its
    /// files owner.
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
        let owner = self.fd_table.read();
        let fd_table = owner.as_ref().ok_or(KError::NoSuchProcess)?;
        access_fn(fd_table.table())
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
    pub fn add_file(&self, file: Arc<VfsFile>, cloexec: bool) -> kerrno::KResult<c_int> {
        self.with_fd_table(|fd_table| fd_table.write().add_file(self.max_nofile(), file, cloexec))
    }

    /// Duplicates a descriptor into a newly allocated slot.
    ///
    /// # Errors
    ///
    /// Propagates source lookup errors from [`Self::get_file`] and insertion errors
    /// from [`Self::add_file`]. The new slot shares the same open file.
    pub fn duplicate_file(&self, fd: c_int, cloexec: bool) -> kerrno::KResult<c_int> {
        let file = self.get_file(fd)?;
        self.add_file(file, cloexec)
    }

    /// Duplicates a descriptor into the lowest free slot at/above `minimum`.
    ///
    /// Reads the soft limit before acquiring the files-owner/table locks. Source
    /// lookup and insertion share one table write guard, and the files-owner
    /// read guard prevents concurrent detachment or replacement during either.
    /// The new slot shares the open file and has the requested close-on-exec bit.
    ///
    /// # Errors
    ///
    /// Returns `KError::NoSuchProcess` after files detachment. Otherwise propagates
    /// source, minimum, and exhaustion errors from [`FdTable::duplicate_from`].
    pub fn duplicate_file_from(&self, fd: c_int, minimum: c_int, cloexec: bool) -> KResult<c_int> {
        let max_nofile = self.max_nofile();
        self.with_fd_table(|fd_table| {
            fd_table
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
        let (owner, fd, replaced) = {
            let mut table = files.table().write();
            let (fd, replaced) = table.duplicate_to(old_fd, new_fd, cloexec)?;
            (table.lock_owner(), fd, replaced)
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

    /// Releases this process's file descriptor table owner.
    ///
    /// The last binding closes its descriptors even when temporary table
    /// references remain. Cleanup runs after releasing the binding lock.
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

    /// Prepares a child's files owner, sharing only when `is_shared` is true.
    ///
    /// The binding guard protects descriptor ownership throughout a private
    /// copy; a temporary table reference alone cannot exclude final cleanup.
    ///
    /// # Errors
    /// Returns `NoSuchProcess` after files detachment.
    pub fn fork_fd_table(&self, is_shared: bool) -> KResult<Arc<FdTableOwner>> {
        let binding = self.fd_table.read();
        let owner = binding.as_ref().ok_or(KError::NoSuchProcess)?;
        Ok(if is_shared {
            owner.clone()
        } else {
            FdTableOwner::copy_from(owner)
        })
    }

    /// Replaces the attached file descriptor table.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] after the files owner is released;
    /// an exited process cannot acquire a new table.
    pub fn replace_fd_table(&self, table: Arc<FdTableOwner>) -> KResult<Arc<FdTableOwner>> {
        let mut owner = self.fd_table.write();
        let old_table = owner.take().ok_or(KError::NoSuchProcess)?;
        *owner = Some(table);
        Ok(old_table)
    }
}

unittest::path_mod_test!(tests, "../ut-tests/lib.rs");
