// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, vec::Vec};
use core::ffi::c_char;

use linux_raw_sys::general::*;
use posix_fs::*;
use posix_types::{IoVec, UserPtr};
use unittest::def_test;
use unittest_support::{TestUserBuffer, TestUserValue};

use super::*;

fn user_string(value: &str) -> TestUserBuffer {
    let buffer = TestUserBuffer::new(value.len() + 1).unwrap();
    buffer.write_bytes(value.as_bytes()).unwrap();
    buffer
}

fn pathname(value: &TestUserBuffer) -> UserConstPtr<c_char> {
    UserConstPtr::from(value.as_user_ptr::<c_char>().cast_const())
}

struct Fixture {
    directory: TestUserBuffer,
    file: TestUserBuffer,
    source: TestUserBuffer,
    fds: Vec<i32>,
}

impl Fixture {
    fn new(name: &str) -> Self {
        // The notifier is kernel-lifetime wiring, identical to the syscall
        // dispatcher's installation. Tests own only their paths and watches.
        crate::notify::install_fs_notifier();
        let directory = user_string(&format!("/tmp/{name}"));
        sys_mkdirat(AT_FDCWD, pathname(&directory), 0o700).unwrap();
        Self {
            directory,
            file: user_string(&format!("/tmp/{name}/probe")),
            source: user_string(&format!("/tmp/{name}/source")),
            fds: Vec::new(),
        }
    }

    fn keep(&mut self, fd: isize) -> i32 {
        self.fds.push(fd as i32);
        fd as i32
    }

    fn close(&mut self, fd: i32) {
        sys_close(fd).unwrap();
        self.fds.retain(|entry| *entry != fd);
    }

    fn open(&mut self, flags: u32) -> i32 {
        let fd = sys_openat(AT_FDCWD, pathname(&self.file), flags as i32, 0o600).unwrap();
        self.keep(fd)
    }

    fn watch(&mut self, mask: u32, watch_directory: bool) -> (i32, i32) {
        let fd = self.keep(sys_inotify_init1(IN_NONBLOCK).unwrap());
        let path = if watch_directory {
            &self.directory
        } else {
            &self.file
        };
        let wd = sys_inotify_add_watch(fd, pathname(path), mask).unwrap() as i32;
        (fd, wd)
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        for fd in self.fds.drain(..) {
            let _ = sys_close(fd);
        }
        let _ = sys_unlinkat(AT_FDCWD, pathname(&self.file), 0);
        let _ = sys_unlinkat(AT_FDCWD, pathname(&self.source), 0);
        let _ = sys_unlinkat(AT_FDCWD, pathname(&self.directory), AT_REMOVEDIR as usize);
    }
}

fn read_event(fd: i32, wd: i32, mask: u32, name: &[u8]) {
    let buffer = TestUserValue::from_value([0u8; 256]).unwrap();
    let size = sys_read(fd, UserPtr::from(buffer.as_user_ptr() as usize), 256).unwrap();
    let bytes = buffer.read();
    let word = |offset: usize| u32::from_ne_bytes(bytes[offset..offset + 4].try_into().unwrap());
    assert_eq!(word(0) as i32, wd);
    assert_eq!(word(4), mask);
    assert_eq!(word(8), 0);
    assert_eq!(size as usize, 16 + word(12) as usize);
    assert_eq!(&bytes[16..16 + name.len()], name);
    if name.is_empty() {
        assert_eq!(word(12), 0);
    } else {
        assert_eq!(bytes[16 + name.len()], 0);
    }
}

fn assert_empty(fd: i32) {
    let buffer = TestUserValue::from_value([0u8; 256]).unwrap();
    assert_eq!(
        sys_read(fd, UserPtr::from(buffer.as_user_ptr() as usize), 256),
        Err(KError::WouldBlock)
    );
}

#[def_test(user, serial)]
fn directory_events_preserve_watch_isolation_and_record_boundaries() {
    let mut fixture = Fixture::new("ik946p-entry-events");
    let (first, first_wd) = fixture.watch(IN_CREATE | IN_DELETE, true);
    let (second, second_wd) = fixture.watch(IN_CREATE | IN_DELETE, true);
    let inode = posix_fs::watch::resolve_watch_target(
        b"/tmp/ik946p-entry-events",
        false,
        true,
        &kprocess::current_cred(),
    )
    .unwrap()
    .inode_ref;
    assert!(inode.watches_notifications(u32::MAX));
    // Re-registering in one instance updates that watch, whereas separate
    // instances own independent subscriptions to the same directory.
    assert_eq!(
        sys_inotify_add_watch(second, pathname(&fixture.directory), IN_CREATE | IN_DELETE),
        Ok(second_wd as isize)
    );
    assert_eq!(
        sys_inotify_add_watch(first, pathname(&fixture.file), IN_MODIFY),
        Err(KError::NotFound)
    );
    fixture.open(O_CREAT | O_EXCL | O_RDWR);
    let short = TestUserValue::from_value([0u8; 16]).unwrap();
    assert_eq!(
        sys_read(first, UserPtr::from(short.as_user_ptr() as usize), 16),
        Err(KError::InvalidInput)
    );
    read_event(first, first_wd, IN_CREATE, b"probe");
    read_event(second, second_wd, IN_CREATE, b"probe");
    sys_inotify_rm_watch(first, first_wd).unwrap();
    assert!(inode.watches_notifications(u32::MAX));
    read_event(first, first_wd, IN_IGNORED, b"");
    sys_unlinkat(AT_FDCWD, pathname(&fixture.file), 0).unwrap();
    assert_empty(first);
    read_event(second, second_wd, IN_DELETE, b"probe");
    fixture.close(second);
    // Duplicate registration and instance teardown must not leak interest.
    assert!(!inode.watches_notifications(u32::MAX));
}

#[def_test(user, serial)]
fn procfs_lookups_preserve_watch_identity_and_content_routing() {
    let mut fixture = Fixture::new("inotify-procfs-identity");
    let paths = [
        "/proc/cmdline",
        "/proc/sys/fs/inotify",
        "/proc/sys/fs/inotify/max_user_watches",
    ];
    let mut watches = Vec::new();
    for path in paths {
        let instance = fixture.keep(sys_inotify_init1(IN_NONBLOCK).unwrap());
        let path = user_string(path);
        let wd = sys_inotify_add_watch(instance, pathname(&path), IN_ATTRIB).unwrap();
        assert_eq!(
            sys_inotify_add_watch(instance, pathname(&path), IN_MODIFY),
            Ok(wd)
        );
        assert_eq!(
            sys_inotify_add_watch(instance, pathname(&path), IN_ATTRIB | IN_MASK_ADD),
            Ok(wd)
        );
        assert_eq!(
            sys_inotify_add_watch(instance, pathname(&path), IN_MODIFY | IN_MASK_CREATE),
            Err(KError::AlreadyExists)
        );
        watches.push((instance, wd as i32));
    }

    let path = user_string(paths[2]);
    let fd = fixture.keep(sys_openat(AT_FDCWD, pathname(&path), O_RDWR as i32, 0).unwrap());
    let buffer = TestUserValue::from_value([0u8; 64]).unwrap();
    let size = sys_read(fd, UserPtr::from(buffer.as_user_ptr() as usize), 64).unwrap();
    assert!(size > 0);
    let input = UserConstPtr::from(buffer.as_user_ptr() as usize);
    // Rewrite the current limit so this test leaves the global budget intact.
    assert_eq!(sys_write(fd, input, size as usize), Ok(size));
    let (parent_instance, parent_wd) = watches[1];
    let (file_instance, file_wd) = watches[2];
    read_event(parent_instance, parent_wd, IN_MODIFY, b"max_user_watches");
    read_event(file_instance, file_wd, IN_MODIFY, b"");
    assert_empty(parent_instance);
    assert_empty(file_instance);
    sys_inotify_rm_watch(file_instance, file_wd).unwrap();
    read_event(file_instance, file_wd, IN_IGNORED, b"");
    assert_eq!(sys_write(fd, input, size as usize), Ok(size));
    read_event(parent_instance, parent_wd, IN_MODIFY, b"max_user_watches");
    assert_empty(parent_instance);
    assert_empty(file_instance);
}

#[def_test(user, serial)]
fn modification_interests_follow_renames_and_watch_removal() {
    let mut original = Fixture::new("inotify-interest-original");
    let mut destination = Fixture::new("inotify-interest-destination");
    let fd = original.open(O_CREAT | O_EXCL | O_RDWR);
    let file = kprocess::current_files().get_file(fd).unwrap();
    let data = TestUserValue::from_value(*b"data").unwrap();
    let input = UserConstPtr::from(data.as_user_ptr() as usize);
    let (dest_watch, dest_wd) = destination.watch(IN_MODIFY, true);
    // A global watch elsewhere must not allocate a parent/name snapshot or
    // route either half of this file's modification event.
    assert!(!file.inode().watches_notifications(u32::MAX));
    assert!(
        file.path()
            .dentry()
            .notification_context(IN_MODIFY)
            .parent
            .is_none()
    );
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    assert_empty(dest_watch);

    let (old_watch, old_wd) = original.watch(IN_MODIFY, true);
    assert!(
        file.path()
            .dentry()
            .notification_context(IN_MODIFY)
            .parent
            .is_some()
    );
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    read_event(old_watch, old_wd, IN_MODIFY, b"probe");
    sys_renameat(
        AT_FDCWD,
        pathname(&original.file),
        AT_FDCWD,
        pathname(&destination.file),
    )
    .unwrap();
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    read_event(dest_watch, dest_wd, IN_MODIFY, b"probe");
    assert_empty(old_watch);

    let (self_watch, self_wd) = destination.watch(IN_MODIFY, false);
    assert!(file.inode().watches_notifications(u32::MAX));
    sys_inotify_rm_watch(dest_watch, dest_wd).unwrap();
    read_event(dest_watch, dest_wd, IN_IGNORED, b"");
    assert!(
        file.path()
            .dentry()
            .notification_context(IN_MODIFY)
            .parent
            .is_none()
    );
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    read_event(self_watch, self_wd, IN_MODIFY, b"");
    assert_empty(dest_watch);

    // Rejected duplicates drop their temporary guard without changing the
    // live mark, and removing that mark clears the inode's final hint.
    assert_eq!(
        sys_inotify_add_watch(
            self_watch,
            pathname(&destination.file),
            IN_MODIFY | IN_MASK_CREATE,
        ),
        Err(KError::AlreadyExists)
    );
    sys_inotify_rm_watch(self_watch, self_wd).unwrap();
    read_event(self_watch, self_wd, IN_IGNORED, b"");
    assert!(!file.inode().watches_notifications(u32::MAX));
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    assert_empty(self_watch);
}

#[def_test(user, serial)]
fn content_changes_notify_directory_and_file_watches() {
    let mut fixture = Fixture::new("ik946p-content-events");
    let fd = fixture.open(O_CREAT | O_EXCL | O_RDWR);
    let source = sys_openat(
        AT_FDCWD,
        pathname(&fixture.source),
        (O_CREAT | O_EXCL | O_RDWR) as i32,
        0o600,
    )
    .unwrap();
    let source = fixture.keep(source);
    let data = TestUserValue::from_value(*b"data").unwrap();
    let input = UserConstPtr::from(data.as_user_ptr() as usize);
    sys_write(source, input, 4).unwrap();
    let (directory, dir_wd) = fixture.watch(IN_MODIFY, true);
    let (file, file_wd) = fixture.watch(IN_MODIFY, false);
    let iov = TestUserValue::from_value(IoVec {
        iov_base: data.as_user_ptr().cast(),
        iov_len: 4,
    })
    .unwrap();
    let iov_ptr = UserConstPtr::from(iov.as_user_ptr() as usize);

    assert_eq!(sys_write(fd, input, 0), Ok(0));
    assert_eq!(
        sys_write(fd, UserConstPtr::default(), 4),
        Err(KError::BadAddress)
    );
    let readonly = fixture.open(O_RDONLY);
    assert_eq!(
        sys_write(readonly, input, 4),
        Err(KError::BadFileDescriptor)
    );
    fixture.open(O_PATH | O_TRUNC);
    assert_empty(directory);
    assert_empty(file);

    for operation in 0..12 {
        match operation {
            0 => {
                assert_eq!(sys_write(fd, input, 4), Ok(4));
            }
            1 => {
                assert_eq!(sys_writev(fd, iov_ptr, 1), Ok(4));
            }
            2 => {
                assert_eq!(sys_pwrite64(fd, input, 4, 0), Ok(4));
            }
            3 => {
                assert_eq!(sys_pwritev2(fd, iov_ptr, 1, 0, 0), Ok(4));
            }
            4 => {
                assert_eq!(sys_pwritev2(fd, iov_ptr, 1, -1, 0), Ok(4));
            }
            5 => {
                sys_ftruncate(fd, 0).unwrap();
            }
            6 => {
                sys_truncate(pathname(&fixture.file), 0).unwrap();
            }
            7 => {
                fixture.open(O_WRONLY | O_TRUNC);
            }
            8 => {
                sys_fallocate(fd, 0, 0, 4).unwrap();
            }
            9 => {
                let input_offset = TestUserValue::from_value(0i64).unwrap();
                let output_offset = TestUserValue::from_value(0i64).unwrap();
                assert_eq!(
                    sys_copy_file_range(
                        source,
                        UserPtr::from(input_offset.as_user_ptr() as usize),
                        fd,
                        UserPtr::from(output_offset.as_user_ptr() as usize),
                        4,
                        0,
                    ),
                    Ok(4)
                );
            }
            10 => {
                let offset = TestUserValue::from_value(0u64).unwrap();
                assert_eq!(
                    sys_sendfile(fd, source, UserPtr::from(offset.as_user_ptr() as usize), 4,),
                    Ok(4)
                );
            }
            11 => {
                let fds = TestUserValue::from_value([0i32; 2]).unwrap();
                crate::ipc::sys_pipe2(UserPtr::from(fds.as_user_ptr() as usize), 0).unwrap();
                let [read, write] = fds.read();
                fixture.keep(read as isize);
                fixture.keep(write as isize);
                assert_eq!(sys_write(write, input, 4), Ok(4));
                assert_eq!(
                    sys_splice(read, UserPtr::default(), fd, UserPtr::default(), 4, 0,),
                    Ok(4)
                );
            }
            _ => unreachable!(),
        }
        read_event(directory, dir_wd, IN_MODIFY, b"probe");
        read_event(file, file_wd, IN_MODIFY, b"");
        assert_empty(directory);
        assert_empty(file);
    }
    let inode = kprocess::current_files()
        .get_file(fd)
        .unwrap()
        .inode()
        .clone();
    sys_unlinkat(AT_FDCWD, pathname(&fixture.file), 0).unwrap();
    read_event(file, file_wd, IN_IGNORED, b"");
    assert!(!inode.watches_notifications(u32::MAX));
}

#[def_test(user, serial)]
fn modification_masks_track_replacement_addition_and_other_instances() {
    let mut fixture = Fixture::new("inotify-mask-interest");
    let fd = fixture.open(O_CREAT | O_EXCL | O_RDWR);
    let file = kprocess::current_files().get_file(fd).unwrap();
    let (first, wd) = fixture.watch(IN_CREATE, true);
    assert!(
        file.path()
            .dentry()
            .notification_context(IN_MODIFY)
            .parent
            .is_none()
    );
    let data = TestUserValue::from_value(*b"data").unwrap();
    let input = UserConstPtr::from(data.as_user_ptr() as usize);
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    assert_empty(first);
    assert_eq!(
        sys_inotify_add_watch(first, pathname(&fixture.directory), IN_MASK_ADD | IN_MODIFY),
        Ok(wd as isize)
    );
    assert!(
        file.path()
            .dentry()
            .notification_context(IN_MODIFY)
            .parent
            .is_some()
    );
    let (second, second_wd) = fixture.watch(IN_MODIFY, true);
    assert_eq!(
        sys_inotify_add_watch(first, pathname(&fixture.directory), IN_CREATE),
        Ok(wd as isize)
    );
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    assert_empty(first);
    read_event(second, second_wd, IN_MODIFY, b"probe");
    fixture.close(second);
    assert!(
        file.path()
            .dentry()
            .notification_context(IN_MODIFY)
            .parent
            .is_none()
    );
    let (own, own_wd) = fixture.watch(IN_ATTRIB, false);
    assert!(!file.inode().watches_notifications(IN_MODIFY));
    sys_inotify_add_watch(own, pathname(&fixture.file), IN_MODIFY).unwrap();
    assert!(file.inode().watches_notifications(IN_MODIFY));
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    read_event(own, own_wd, IN_MODIFY, b"");
    sys_inotify_rm_watch(own, own_wd).unwrap();
    assert!(!file.inode().watches_notifications(IN_MODIFY));
}

struct BindMount {
    target: TestUserBuffer,
}

impl BindMount {
    fn new(source: &TestUserBuffer, target: &str) -> Self {
        let target = user_string(target);
        sys_mount(
            pathname(source),
            pathname(&target),
            UserConstPtr::from(0usize),
            MS_BIND,
            UserConstPtr::from(0usize),
        )
        .unwrap();
        Self { target }
    }
}

impl Drop for BindMount {
    fn drop(&mut self) {
        // Detach also permits cleanup when a failed assertion leaves an open
        // descriptor referring to the test mount.
        let _ = sys_umount2(pathname(&self.target), MNT_DETACH as i32);
    }
}

#[def_test(user, serial)]
fn directory_bind_routes_content_and_namespace_events_to_original_objects() {
    let mut source = Fixture::new("inotify-bind-source");
    let mut alias = Fixture::new("inotify-bind-alias");
    source.open(O_CREAT | O_EXCL | O_RDWR);
    let (directory, directory_wd) = source.watch(IN_MODIFY | IN_CREATE | IN_DELETE, true);
    let (own, own_wd) = source.watch(IN_MODIFY | IN_MOVE_SELF, false);
    let _mount = BindMount::new(&source.directory, "/tmp/inotify-bind-alias");
    assert_eq!(
        sys_inotify_add_watch(own, pathname(&alias.file), IN_MASK_ADD | IN_MODIFY),
        Ok(own_wd as isize)
    );
    assert_eq!(
        sys_inotify_add_watch(
            directory,
            pathname(&alias.directory),
            IN_MASK_CREATE | IN_MODIFY
        ),
        Err(KError::AlreadyExists)
    );
    let fd = alias.open(O_RDWR);
    let data = TestUserValue::from_value(*b"data").unwrap();
    let input = UserConstPtr::from(data.as_user_ptr() as usize);
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    read_event(directory, directory_wd, IN_MODIFY, b"probe");
    read_event(own, own_wd, IN_MODIFY, b"");
    let created = sys_openat(
        AT_FDCWD,
        pathname(&alias.source),
        (O_CREAT | O_EXCL | O_RDWR) as i32,
        0o600,
    )
    .unwrap();
    alias.keep(created);
    read_event(directory, directory_wd, IN_CREATE, b"source");
    sys_unlinkat(AT_FDCWD, pathname(&alias.source), 0).unwrap();
    read_event(directory, directory_wd, IN_DELETE, b"source");
    sys_renameat(
        AT_FDCWD,
        pathname(&alias.file),
        AT_FDCWD,
        pathname(&alias.source),
    )
    .unwrap();
    read_event(own, own_wd, IN_MOVE_SELF, b"");
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    read_event(directory, directory_wd, IN_MODIFY, b"source");
    read_event(own, own_wd, IN_MODIFY, b"");
    sys_unlinkat(AT_FDCWD, pathname(&alias.source), 0).unwrap();
    read_event(directory, directory_wd, IN_DELETE, b"source");
    read_event(own, own_wd, IN_IGNORED, b"");
}

#[def_test(user, serial)]
fn file_bind_keeps_the_source_dentry_parent() {
    let mut source = Fixture::new("inotify-file-bind-source");
    let mut alias = Fixture::new("inotify-file-bind-alias");
    source.open(O_CREAT | O_EXCL | O_RDWR);
    alias.open(O_CREAT | O_EXCL | O_RDWR);
    let (directory, directory_wd) = source.watch(IN_MODIFY, true);
    let (alias_directory, _) = alias.watch(IN_MODIFY, true);
    let (own, own_wd) = source.watch(IN_MODIFY, false);
    let _mount = BindMount::new(&source.file, "/tmp/inotify-file-bind-alias/probe");
    assert_eq!(
        sys_inotify_add_watch(own, pathname(&alias.file), IN_MODIFY),
        Ok(own_wd as isize)
    );
    let fd = alias.open(O_RDWR);
    let data = TestUserValue::from_value(*b"data").unwrap();
    assert_eq!(
        sys_write(fd, UserConstPtr::from(data.as_user_ptr() as usize), 4),
        Ok(4)
    );
    read_event(directory, directory_wd, IN_MODIFY, b"probe");
    read_event(own, own_wd, IN_MODIFY, b"");
    assert_empty(alias_directory);
}

#[def_test(user, serial)]
fn special_file_content_notifies_only_the_object() {
    let mut fixture = Fixture::new("inotify-fifo-content");
    sys_mknodat(AT_FDCWD, pathname(&fixture.file), S_IFIFO | 0o600, 0).unwrap();
    let fd = fixture.open(O_RDWR | O_NONBLOCK);
    let (directory, _) = fixture.watch(IN_MODIFY, true);
    let (own, own_wd) = fixture.watch(IN_MODIFY, false);
    let data = TestUserValue::from_value(*b"data").unwrap();
    let input = UserConstPtr::from(data.as_user_ptr() as usize);
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    assert_empty(directory);
    read_event(own, own_wd, IN_MODIFY, b"");

    let dev = user_string("/dev");
    let null = user_string("/dev/null");
    let directory = fixture.keep(sys_inotify_init1(IN_NONBLOCK).unwrap());
    sys_inotify_add_watch(directory, pathname(&dev), IN_MODIFY).unwrap();
    let own = fixture.keep(sys_inotify_init1(IN_NONBLOCK).unwrap());
    let own_wd = sys_inotify_add_watch(own, pathname(&null), IN_MODIFY).unwrap() as i32;
    let fd = fixture.keep(sys_openat(AT_FDCWD, pathname(&null), O_WRONLY as i32, 0).unwrap());
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    assert_empty(directory);
    read_event(own, own_wd, IN_MODIFY, b"");
}

#[def_test(user, serial)]
fn excl_unlink_filters_only_the_removed_directory_entry() {
    let mut fixture = Fixture::new("inotify-excl-unlink");
    let fd = fixture.open(O_CREAT | O_EXCL | O_RDWR);
    sys_linkat(
        AT_FDCWD,
        pathname(&fixture.file),
        AT_FDCWD,
        pathname(&fixture.source),
        0,
    )
    .unwrap();
    let (all, all_wd) = fixture.watch(IN_MODIFY, true);
    let (linked, linked_wd) = fixture.watch(IN_MODIFY | IN_DELETE | IN_EXCL_UNLINK, true);
    sys_unlinkat(AT_FDCWD, pathname(&fixture.file), 0).unwrap();
    read_event(linked, linked_wd, IN_DELETE, b"probe");
    let data = TestUserValue::from_value(*b"data").unwrap();
    let input = UserConstPtr::from(data.as_user_ptr() as usize);
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    read_event(all, all_wd, IN_MODIFY, b"probe");
    assert_empty(linked);
    let remaining =
        fixture.keep(sys_openat(AT_FDCWD, pathname(&fixture.source), O_RDWR as i32, 0).unwrap());
    assert_eq!(sys_write(remaining, input, 4), Ok(4));
    read_event(all, all_wd, IN_MODIFY, b"source");
    read_event(linked, linked_wd, IN_MODIFY, b"source");
    // Recreating the old name must not make the old open dentry linked again.
    fixture.open(O_CREAT | O_EXCL | O_RDWR);
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    read_event(all, all_wd, IN_MODIFY, b"probe");
    assert_empty(linked);
    sys_unlinkat(AT_FDCWD, pathname(&fixture.source), 0).unwrap();
    read_event(linked, linked_wd, IN_DELETE, b"source");
    assert_eq!(sys_write(fd, input, 4), Ok(4));
    read_event(all, all_wd, IN_MODIFY, b"probe");
    assert_empty(linked);
}

#[def_test(user, serial)]
fn excl_unlink_filters_replaced_targets_but_preserves_moved_sources() {
    let mut fixture = Fixture::new("inotify-excl-replace");
    let replaced = fixture.open(O_CREAT | O_EXCL | O_RDWR);
    let source = fixture.keep(
        sys_openat(
            AT_FDCWD,
            pathname(&fixture.source),
            (O_CREAT | O_EXCL | O_RDWR) as i32,
            0o600,
        )
        .unwrap(),
    );
    let (all, all_wd) = fixture.watch(IN_MODIFY, true);
    let (linked, linked_wd) = fixture.watch(IN_MODIFY | IN_EXCL_UNLINK, true);
    sys_renameat(
        AT_FDCWD,
        pathname(&fixture.source),
        AT_FDCWD,
        pathname(&fixture.file),
    )
    .unwrap();
    let data = TestUserValue::from_value(*b"data").unwrap();
    let input = UserConstPtr::from(data.as_user_ptr() as usize);
    assert_eq!(sys_write(replaced, input, 4), Ok(4));
    read_event(all, all_wd, IN_MODIFY, b"probe");
    assert_empty(linked);
    assert_eq!(sys_write(source, input, 4), Ok(4));
    read_event(all, all_wd, IN_MODIFY, b"probe");
    read_event(linked, linked_wd, IN_MODIFY, b"probe");

    // Exchange moves both live dentries; neither becomes an unlinked target.
    let other = fixture.keep(
        sys_openat(
            AT_FDCWD,
            pathname(&fixture.source),
            (O_CREAT | O_EXCL | O_RDWR) as i32,
            0o600,
        )
        .unwrap(),
    );
    sys_renameat2(
        AT_FDCWD,
        pathname(&fixture.file),
        AT_FDCWD,
        pathname(&fixture.source),
        RENAME_EXCHANGE,
    )
    .unwrap();
    assert_eq!(sys_write(source, input, 4), Ok(4));
    read_event(all, all_wd, IN_MODIFY, b"source");
    read_event(linked, linked_wd, IN_MODIFY, b"source");
    assert_eq!(sys_write(other, input, 4), Ok(4));
    read_event(all, all_wd, IN_MODIFY, b"probe");
    read_event(linked, linked_wd, IN_MODIFY, b"probe");
}
