// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::ffi::{c_char, c_int};

use kerrno::KError;
use linux_raw_sys::general::*;
use posix_types::UserConstPtr;
use unittest::{assert_eq, def_test};
use unittest_support::TestUserArray;

use super::*;

/// Returns the permission bits and size of `fd` via descriptor-only resolution,
/// mirroring `sys_fstat`.
fn stat_of(fd: c_int) -> (u32, u64) {
    let stat = crate::path::resolve_at(fd, None, AT_EMPTY_PATH)
        .unwrap()
        .stat()
        .unwrap();
    (stat.mode & 0o7777, stat.size)
}

#[def_test(user)]
fn creat_creates_file_and_returns_write_only_fd() {
    let path = TestUserArray::from_array(*b"/tmp/ut-creat-new\0").unwrap();
    let name = UserConstPtr::from(path.as_user_ptr().cast::<c_char>().cast_const());
    let _ = crate::sys_unlinkat(AT_FDCWD, name, 0);

    let fd = sys_creat(name, 0o644).unwrap() as c_int;
    assert!(fd >= 0);
    let flags = crate::sys_fcntl(fd, F_GETFL as _, 0).unwrap();
    assert_eq!(flags & (O_ACCMODE as isize), O_WRONLY as isize);
    let (mode, size) = stat_of(fd);
    assert_eq!(size, 0);
    assert_eq!(mode, 0o644 & !kprocess::current_umask());

    crate::sys_close(fd).unwrap();
    crate::sys_unlinkat(AT_FDCWD, name, 0).unwrap();
}

#[def_test(user)]
fn creat_truncates_existing_content() {
    let path = TestUserArray::from_array(*b"/tmp/ut-creat-trunc\0").unwrap();
    let name = UserConstPtr::from(path.as_user_ptr().cast::<c_char>().cast_const());
    let _ = crate::sys_unlinkat(AT_FDCWD, name, 0);

    let fd = sys_openat(AT_FDCWD, name, (O_CREAT | O_RDWR) as i32, 0o600).unwrap() as c_int;
    let content = TestUserArray::from_array(*b"0123456789\0").unwrap();
    assert_eq!(
        crate::sys_write(fd, UserConstPtr::from(content.as_user_ptr().cast_const()), 10),
        Ok(10)
    );
    crate::sys_close(fd).unwrap();

    let fd = sys_creat(name, 0o600).unwrap() as c_int;
    assert_eq!(stat_of(fd).1, 0);
    crate::sys_close(fd).unwrap();
    crate::sys_unlinkat(AT_FDCWD, name, 0).unwrap();
}

#[def_test(user)]
fn creat_rejects_bad_arguments() {
    assert_eq!(sys_creat(UserConstPtr::default(), 0o600), Err(KError::BadAddress));

    let path = TestUserArray::from_array(*b"/ut-creat-missing-dir/f\0").unwrap();
    let name = UserConstPtr::from(path.as_user_ptr().cast::<c_char>().cast_const());
    assert_eq!(sys_creat(name, 0o600), Err(KError::NotFound));
}
