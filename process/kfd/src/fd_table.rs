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

use crate::{FdSnapshot, FileDescriptor, FileLike};

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

    /// Returns a stable snapshot of the descriptor entry.
    pub fn snapshot(&self, fd: c_int) -> KResult<FdSnapshot> {
        self.get(fd as usize)
            .map(|descriptor| descriptor.snapshot(fd))
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

#[cfg(unittest)]
mod unittest_tests {
    use alloc::{borrow::Cow, sync::Arc, vec, vec::Vec};
    use core::task::Context;

    use kerrno::KError;
    use kpoll::{IoEvents, Pollable};
    use unittest::def_test;

    use super::*;

    #[derive(Debug)]
    struct MockFile {
        name: &'static str,
        ready: IoEvents,
    }

    impl Pollable for MockFile {
        fn poll(&self) -> IoEvents {
            self.ready
        }

        fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
    }

    impl FileLike for MockFile {
        fn path(&self) -> Cow<'_, str> {
            Cow::Borrowed(self.name)
        }
    }

    #[derive(Debug)]
    struct OtherFile;

    impl Pollable for OtherFile {
        fn poll(&self) -> IoEvents {
            IoEvents::empty()
        }

        fn register(&self, _context: &mut Context<'_>, _events: IoEvents) {}
    }

    impl FileLike for OtherFile {
        fn path(&self) -> Cow<'_, str> {
            Cow::Borrowed("other")
        }
    }

    fn mock_file(name: &'static str) -> Arc<dyn FileLike> {
        Arc::new(MockFile {
            name,
            ready: IoEvents::IN,
        })
    }

    #[def_test]
    fn add_get_and_downcast_file_like() {
        let mut table = FdTable::default();
        let fd = table.add_file_like(4, mock_file("alpha"), false).unwrap();
        assert_eq!(fd, 0);
        assert_eq!(table.count(), 1);
        assert_eq!(table.ids().collect::<Vec<_>>(), vec![0]);
        assert_eq!(table.get_file_like(fd).unwrap().path(), "alpha");
        assert!(
            table
                .get_file_like_as::<MockFile>(fd)
                .unwrap()
                .poll()
                .contains(IoEvents::IN)
        );
        assert!(matches!(
            table.get_file_like_as::<OtherFile>(fd),
            Err(KError::InvalidInput)
        ));
        assert!(matches!(
            table.get_file_like(9),
            Err(KError::BadFileDescriptor)
        ));
    }

    #[def_test]
    fn add_and_insert_enforce_limits_and_slots() {
        let mut table = FdTable::default();
        assert_eq!(
            table.add_file_like(0, mock_file("overflow"), false),
            Err(KError::TooManyOpenFiles)
        );

        let fd0 = match table.insert_file_like(mock_file("first"), false) {
            Ok(fd) => fd,
            Err(_) => panic!("insert_file_like should allocate the first slot"),
        };
        let fd5 = match table.add_at(5, FileDescriptor::new(mock_file("fixed"), true)) {
            Ok(fd) => fd,
            Err(_) => panic!("add_at should insert into an empty fixed slot"),
        };
        assert_eq!(fd0, 0);
        assert_eq!(fd5, 5);
        assert_eq!(table.ids().collect::<Vec<_>>(), vec![0, 5]);
        assert!(
            table
                .add_at(5, FileDescriptor::new(mock_file("dup"), false))
                .is_err()
        );
    }

    #[def_test]
    fn duplicate_and_cloexec_operations_update_descriptor_state() {
        let mut table = FdTable::default();
        let old_fd = table.add_file_like(8, mock_file("src"), false).unwrap();
        let replaced_fd = table.add_file_like(8, mock_file("victim"), true).unwrap();
        assert_eq!(replaced_fd, 1);

        assert_eq!(table.cloexec(old_fd).unwrap(), false);
        table.set_cloexec(old_fd, true).unwrap();
        assert_eq!(table.cloexec(old_fd).unwrap(), true);

        let new_fd = table.duplicate_to(old_fd, replaced_fd, false).unwrap();
        assert_eq!(new_fd, replaced_fd);
        assert_eq!(table.cloexec(new_fd).unwrap(), false);
        assert_eq!(table.get_file_like(new_fd).unwrap().path(), "src");
        assert_eq!(
            table.duplicate_to(99, 4, false),
            Err(KError::BadFileDescriptor)
        );
        assert_eq!(table.set_cloexec(99, true), Err(KError::BadFileDescriptor));
        assert_eq!(table.cloexec(99), Err(KError::BadFileDescriptor));
    }

    #[def_test]
    fn close_and_range_operations_only_touch_selected_entries() {
        let mut table = FdTable::default();
        for name in ["a", "b", "c", "d"] {
            table.add_file_like(8, mock_file(name), false).unwrap();
        }

        table.set_cloexec_range(1, 8);
        assert_eq!(table.cloexec(0).unwrap(), false);
        assert_eq!(table.cloexec(1).unwrap(), true);
        assert_eq!(table.cloexec(3).unwrap(), true);

        table.close_range(1, 2);
        assert_eq!(table.ids().collect::<Vec<_>>(), vec![0, 3]);
        assert_eq!(table.close_file_like(2), Err(KError::BadFileDescriptor));
        table.close_cloexec_files();
        assert_eq!(table.ids().collect::<Vec<_>>(), vec![0]);
        table.close_range(10, 12);
        assert_eq!(table.count(), 1);
    }

    #[def_test]
    fn clone_shared_and_close_all_if_unshared_follow_reference_count() {
        let shared = FdTable::new_shared();
        {
            let mut guard = shared.write();
            guard.add_file_like(4, mock_file("first"), false).unwrap();
            guard.add_file_like(4, mock_file("second"), true).unwrap();
        }

        let cloned = FdTable::clone_shared_from(&shared);
        assert_eq!(cloned.read().ids().collect::<Vec<_>>(), vec![0, 1]);
        assert_eq!(cloned.read().get_file_like(1).unwrap().path(), "second");

        let extra_ref = Arc::clone(&shared);
        FdTable::close_all_if_unshared(&shared);
        assert_eq!(shared.read().count(), 2);
        drop(extra_ref);

        FdTable::close_all_if_unshared(&shared);
        assert_eq!(shared.read().count(), 0);
        assert_eq!(cloned.read().count(), 2);
        drop(cloned);
    }
}
