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
use kfd::{FdSnapshot, FdTable, FileDescriptor};
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
    fd_table: RwLock<Option<Arc<RwLock<FdTable>>>>,
}

impl ProcessResources {
    /// Creates a new process resource set with default limits.
    ///
    /// `user_stack_size` is the configured user stack capacity in bytes,
    /// passed to [`Rlimits::new`]. The attached descriptor table starts empty.
    pub fn new(user_stack_size: usize) -> Arc<Self> {
        Arc::new(Self {
            rlimits: RwLock::new(Rlimits::new(user_stack_size)),
            fd_table: RwLock::new(Some(FdTable::new_shared())),
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

    /// Returns the current soft/hard pair for a specific resource.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` when `resource >= RLIM_NLIMITS`.
    pub fn rlimit_values(&self, resource: u32) -> KResult<(u64, u64)> {
        let limit = self.rlimit(resource)?;
        Ok((limit.current, limit.max))
    }

    /// Updates the limit for a specific resource.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for an out-of-range resource or soft > hard.
    /// Returns `KError::OperationNotPermitted` for any hard-limit increase.
    /// The old pair is unchanged on failure; capability-based increases are not supported.
    pub fn set_rlimit(&self, resource: u32, new_limit: Rlimit) -> KResult {
        if resource >= RLIM_NLIMITS {
            return Err(KError::InvalidInput);
        }
        if new_limit.current > new_limit.max {
            return Err(KError::InvalidInput);
        }

        let limit = &mut self.rlimits.write()[resource];
        if new_limit.max > limit.max {
            // Raising the hard limit requires CAP_SYS_RESOURCE.
            // Return EPERM until proper credential checks are in place.
            return Err(KError::OperationNotPermitted);
        }

        *limit = new_limit;
        Ok(())
    }

    /// Updates the soft/hard pair for a specific resource.
    ///
    /// # Errors
    ///
    /// Returns `KError::InvalidInput` for an out-of-range resource or soft > hard.
    /// Returns `KError::OperationNotPermitted` for any hard-limit increase.
    /// The old pair is unchanged on failure; capability-based increases are not supported.
    pub fn set_rlimit_values(&self, resource: u32, current: u64, max: u64) -> KResult {
        self.set_rlimit(resource, Rlimit::new(current, max))
    }

    /// Returns the attached file descriptor table.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] after the process has released its
    /// files owner.
    pub fn fd_table(&self) -> KResult<Arc<RwLock<FdTable>>> {
        self.fd_table.read().clone().ok_or(KError::NoSuchProcess)
    }

    fn with_fd_table<R>(
        &self,
        access_fn: impl FnOnce(&RwLock<FdTable>) -> KResult<R>,
    ) -> KResult<R> {
        let owner = self.fd_table.read();
        let fd_table = owner.as_ref().ok_or(KError::NoSuchProcess)?;
        access_fn(fd_table)
    }

    fn close_descriptors(descriptors: impl IntoIterator<Item = FileDescriptor>) {
        for descriptor in descriptors {
            // Teardown must close every descriptor; close errors have no receiver.
            let _ = descriptor.close();
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
        let (fd, replaced) =
            self.with_fd_table(|fd_table| fd_table.write().duplicate_to(old_fd, new_fd, cloexec))?;
        Self::close_descriptors(replaced);
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
        let descriptor =
            self.with_fd_table(|fd_table| fd_table.write().file_close_fd_locked(fd))?;
        descriptor.close()
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
        let descriptors =
            self.with_fd_table(|fd_table| Ok(fd_table.write().remove_range(first_fd, last_fd)))?;
        Self::close_descriptors(descriptors);
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
        let descriptors =
            self.with_fd_table(|fd_table| Ok(fd_table.write().remove_cloexec_files()))?;
        Self::close_descriptors(descriptors);
        Ok(())
    }

    /// Releases this process's file descriptor table owner.
    ///
    /// A table shared by multiple processes closes its descriptors when its
    /// final owner is released.
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

            let new_table = FdTable::clone_shared_from(table);
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

    /// Replaces the attached file descriptor table.
    ///
    /// # Errors
    ///
    /// Returns [`KError::NoSuchProcess`] after the files owner is released;
    /// an exited process cannot acquire a new table.
    pub fn replace_fd_table(&self, table: Arc<RwLock<FdTable>>) -> KResult<Arc<RwLock<FdTable>>> {
        let mut owner = self.fd_table.write();
        let old_table = owner.take().ok_or(KError::NoSuchProcess)?;
        *owner = Some(table);
        Ok(old_table)
    }
}

#[unittest::mod_test]
mod tests {
    use alloc::sync::Arc;

    use krlimit::Rlimit;
    use unittest::def_test;

    use super::{FdTable, ProcessResources};

    #[def_test]
    fn test_process_resources_default_limits() {
        let resources = ProcessResources::new(0x80000);
        assert_eq!(resources.max_nofile(), 1024);
        assert_eq!(
            resources.rlimits.read()[linux_raw_sys::general::RLIMIT_STACK].current,
            0x80000
        );
        assert_eq!(resources.fd_table().unwrap().read().count(), 0);
    }

    #[def_test]
    fn test_rlimit_accessors_update_selected_limit() {
        let resources = ProcessResources::new(0x80000);

        assert_eq!(
            resources
                .rlimit(linux_raw_sys::general::RLIMIT_NOFILE)
                .unwrap(),
            Rlimit::new(1024, 1024)
        );
        assert_eq!(
            resources
                .rlimit_values(linux_raw_sys::general::RLIMIT_NOFILE)
                .unwrap(),
            (1024, 1024)
        );

        resources
            .set_rlimit(
                linux_raw_sys::general::RLIMIT_NOFILE,
                Rlimit::new(512, 1024),
            )
            .unwrap();

        assert_eq!(
            resources
                .rlimit(linux_raw_sys::general::RLIMIT_NOFILE)
                .unwrap(),
            Rlimit::new(512, 1024)
        );
        assert_eq!(
            resources
                .rlimit_values(linux_raw_sys::general::RLIMIT_NOFILE)
                .unwrap(),
            (512, 1024)
        );
    }

    #[def_test]
    fn test_set_rlimit_rejects_invalid_or_privileged_updates() {
        let resources = ProcessResources::new(0x80000);

        assert_eq!(
            resources
                .set_rlimit(
                    linux_raw_sys::general::RLIMIT_NOFILE,
                    Rlimit::new(2048, 1024)
                )
                .unwrap_err(),
            kerrno::KError::InvalidInput
        );

        assert_eq!(
            resources
                .set_rlimit(
                    linux_raw_sys::general::RLIMIT_NOFILE,
                    Rlimit::new(1024, 2048)
                )
                .unwrap_err(),
            kerrno::KError::OperationNotPermitted
        );
    }

    #[def_test]
    fn test_exit_files_detaches_fd_table() {
        let resources = ProcessResources::new(0x80000);

        resources.exit_files();

        assert_eq!(
            resources.fd_table().err(),
            Some(kerrno::KError::NoSuchProcess)
        );
        assert_eq!(
            resources.replace_fd_table(FdTable::new_shared()).err(),
            Some(kerrno::KError::NoSuchProcess)
        );
        resources.exit_files();
    }

    #[def_test]
    fn test_duplicate_file_from_rejects_detached_owner() {
        let resources = ProcessResources::new(0x80000);
        resources.exit_files();
        assert_eq!(
            resources.duplicate_file_from(0, 0, false),
            Err(kerrno::KError::NoSuchProcess)
        );
    }

    #[def_test]
    fn test_shared_fd_table_lives_until_last_files_owner_exits() {
        let first = ProcessResources::new(0x80000);
        let second = ProcessResources::new(0x80000);
        let shared = first.fd_table().unwrap();
        let weak = Arc::downgrade(&shared);
        drop(second.replace_fd_table(shared).unwrap());

        first.exit_files();
        assert!(weak.upgrade().is_some());

        second.exit_files();
        assert!(weak.upgrade().is_none());
    }
}
