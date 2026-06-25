// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process-owned resource objects.

#![no_std]

extern crate alloc;

use alloc::sync::Arc;
use core::ffi::c_int;

use kerrno::{KError, KResult};
use kfd::{FdSnapshot, FdTable, FileLike};
use krlimit::{Rlimit, Rlimits};
use ksync::RwLock;
use linux_raw_sys::general::{RLIM_NLIMITS, RLIMIT_NOFILE};

/// Process-owned resource state.
///
/// This is the first owner boundary for process resources. More resource handles
/// can move here later; for now it owns the rlimit set explicitly.
pub struct ProcessResources {
    /// Per-process resource limits.
    pub rlimits: RwLock<Rlimits>,
    /// The process-owned file descriptor table handle.
    fd_table: RwLock<Arc<RwLock<FdTable>>>,
}

impl ProcessResources {
    /// Creates a new process resource set with default limits.
    pub fn new(user_stack_size: usize) -> Arc<Self> {
        Arc::new(Self {
            rlimits: RwLock::new(Rlimits::new(user_stack_size)),
            fd_table: RwLock::new(FdTable::new_shared()),
        })
    }

    /// Returns the current RLIMIT_NOFILE soft cap.
    pub fn max_nofile(&self) -> u64 {
        self.rlimits.read()[RLIMIT_NOFILE].current
    }

    /// Returns the current limit for a specific resource.
    pub fn rlimit(&self, resource: u32) -> KResult<Rlimit> {
        if resource >= RLIM_NLIMITS {
            return Err(KError::InvalidInput);
        }

        Ok(self.rlimits.read()[resource])
    }

    /// Returns the current soft/hard pair for a specific resource.
    pub fn rlimit_values(&self, resource: u32) -> KResult<(u64, u64)> {
        let limit = self.rlimit(resource)?;
        Ok((limit.current, limit.max))
    }

    /// Updates the limit for a specific resource.
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
    pub fn set_rlimit_values(&self, resource: u32, current: u64, max: u64) -> KResult {
        self.set_rlimit(resource, Rlimit::new(current, max))
    }

    /// Returns the current file descriptor table handle.
    pub fn fd_table(&self) -> Arc<RwLock<FdTable>> {
        self.fd_table.read().clone()
    }

    fn with_fd_table<R>(&self, access_fn: impl FnOnce(&RwLock<FdTable>) -> R) -> R {
        let fd_table = self.fd_table.read();
        access_fn((*fd_table).as_ref())
    }

    /// Returns the file-like object stored in the given descriptor.
    pub fn get_file_like(&self, fd: c_int) -> kerrno::KResult<Arc<dyn FileLike>> {
        self.with_fd_table(|fd_table| fd_table.read().get_file_like(fd))
    }

    /// Returns a stable snapshot of the descriptor entry.
    pub fn snapshot_fd(&self, fd: c_int) -> kerrno::KResult<FdSnapshot> {
        self.with_fd_table(|fd_table| fd_table.read().snapshot(fd))
    }

    /// Returns the typed file-like object stored in the given descriptor.
    pub fn get_file_like_as<T: FileLike + 'static>(&self, fd: c_int) -> kerrno::KResult<Arc<T>> {
        self.with_fd_table(|fd_table| T::from_fd(fd_table, fd))
    }

    /// Adds a file-like object to the current descriptor table.
    pub fn add_file_like(
        &self,
        file_like: Arc<dyn FileLike>,
        cloexec: bool,
    ) -> kerrno::KResult<c_int> {
        self.with_fd_table(|fd_table| {
            fd_table
                .write()
                .add_file_like(self.max_nofile(), file_like, cloexec)
        })
    }

    /// Duplicates a descriptor into a newly allocated slot.
    pub fn duplicate_file_like(&self, fd: c_int, cloexec: bool) -> kerrno::KResult<c_int> {
        let file_like = self.get_file_like(fd)?;
        self.add_file_like(file_like, cloexec)
    }

    /// Duplicates a descriptor into a fixed slot.
    pub fn duplicate_file_like_to(
        &self,
        old_fd: c_int,
        new_fd: c_int,
        cloexec: bool,
    ) -> kerrno::KResult<c_int> {
        self.with_fd_table(|fd_table| fd_table.write().duplicate_to(old_fd, new_fd, cloexec))
    }

    /// Closes the given file descriptor.
    pub fn close_file_like(&self, fd: c_int) -> kerrno::KResult {
        self.with_fd_table(|fd_table| fd_table.write().close_file_like(fd))
    }

    /// Returns whether the given descriptor is marked close-on-exec.
    pub fn cloexec(&self, fd: c_int) -> kerrno::KResult<bool> {
        self.with_fd_table(|fd_table| fd_table.read().cloexec(fd))
    }

    /// Updates the close-on-exec bit for the given descriptor.
    pub fn set_cloexec(&self, fd: c_int, cloexec: bool) -> kerrno::KResult {
        self.with_fd_table(|fd_table| fd_table.write().set_cloexec(fd, cloexec))
    }

    /// Closes all descriptors in the given inclusive range.
    pub fn close_range(&self, first_fd: c_int, last_fd: c_int) {
        self.with_fd_table(|fd_table| fd_table.write().close_range(first_fd, last_fd));
    }

    /// Marks all descriptors in the given inclusive range close-on-exec.
    pub fn set_cloexec_range(&self, first_fd: c_int, last_fd: c_int) {
        self.with_fd_table(|fd_table| fd_table.write().set_cloexec_range(first_fd, last_fd));
    }

    /// Closes all descriptors marked close-on-exec.
    pub fn close_cloexec_files(&self) {
        self.with_fd_table(|fd_table| fd_table.write().close_cloexec_files());
    }

    /// Closes all file descriptors when the table is not shared.
    pub fn close_all_fds(&self) {
        // Must NOT call self.fd_table() — that clones the inner Arc and
        // bumps strong_count, tricking close_all_if_unshared into returning
        // early without closing any descriptors.
        let guard = self.fd_table.read();
        FdTable::close_all_if_unshared(&guard);
    }

    /// Replaces the current fd table with an unshared clone.
    pub fn unshare_fd_table(&self) {
        let old_table = self.fd_table();
        let new_table = FdTable::clone_shared_from(&old_table);
        self.replace_fd_table(new_table);
    }

    /// Replaces the file descriptor table handle.
    pub fn replace_fd_table(&self, table: Arc<RwLock<FdTable>>) -> Arc<RwLock<FdTable>> {
        core::mem::replace(&mut *self.fd_table.write(), table)
    }
}

#[cfg(unittest)]
mod tests {
    use krlimit::Rlimit;
    use unittest::def_test;

    use super::ProcessResources;

    #[def_test]
    fn test_process_resources_default_limits() {
        let resources = ProcessResources::new(0x80000);
        assert_eq!(resources.max_nofile(), 1024);
        assert_eq!(
            resources.rlimits.read()[linux_raw_sys::general::RLIMIT_STACK].current,
            0x80000
        );
        assert_eq!(resources.fd_table().read().count(), 0);
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
}
