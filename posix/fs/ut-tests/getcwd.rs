// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};
use unittest_support::TestUserArray;

use super::*;

fn enter_root() {
    // Each user test owns its copied FsStruct; this does not change the
    // initial filesystem context or another test's working directory.
    let context = kprocess::current_user_process().fs_context().unwrap();
    let mut fs = context.lock();
    let root = fs.root().clone();
    fs.set_pwd(root).unwrap();
}

#[def_test(user)]
fn getcwd_writes_only_the_path_for_unsigned_capacities() {
    enter_root();
    let mut buffer = TestUserArray::from_array([b'!'; 8]).unwrap();
    for size in [2, 8, usize::MAX] {
        buffer.write([b'!'; 8]);
        assert_eq!(sys_getcwd(UserPtr::from(buffer.as_user_ptr()), size), Ok(2));
        assert_eq!(buffer.read(), *b"/\0!!!!!!");
    }
}

#[def_test(user)]
fn getcwd_rejects_short_capacity_before_accessing_output() {
    enter_root();
    let buffer = TestUserArray::from_array([b'!'; 8]).unwrap();
    for size in [0, 1] {
        assert_eq!(
            sys_getcwd(UserPtr::from(buffer.as_user_ptr()), size),
            Err(KError::OutOfRange)
        );
        assert_eq!(buffer.read(), [b'!'; 8]);
        assert_eq!(
            sys_getcwd(UserPtr::default(), size),
            Err(KError::OutOfRange)
        );
    }
}

#[def_test(user)]
fn getcwd_rejects_inaccessible_output_with_adequate_capacity() {
    enter_root();
    // The test-owned user address space leaves its first page unmapped.
    for address in [0_usize, 1] {
        for size in [2, 16, usize::MAX] {
            assert_eq!(
                sys_getcwd(UserPtr::from(address), size),
                Err(KError::BadAddress)
            );
        }
    }
}
