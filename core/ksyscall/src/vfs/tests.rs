// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! File-lock ABI checks through syscall entry points and user memory.

use kerrno::{KError, LinuxError};
use linux_raw_sys::general::*;
use posix_types::{UserConstPtr, fs::k_flock64};
use unittest::def_test;
use unittest_support::TestUserValue;

use super::{sys_close, sys_dup, sys_fcntl};

fn memfd() -> i32 {
    let name = TestUserValue::from_value(*b"lock-test\0").unwrap();
    posix_mm::sys_memfd_create(UserConstPtr::from(name.as_user_ptr() as usize), 0).unwrap() as i32
}

fn flock(kind: u32, start: i64, len: i64, pid: i32) -> k_flock64 {
    let mut lock = k_flock64::default();
    lock.l_type = kind as _;
    lock.l_whence = SEEK_SET as _;
    lock.l_start = start;
    lock.l_len = len;
    lock.l_pid = pid;
    lock
}

#[def_test(user)]
fn getlk_validation_and_unlocked_output() {
    let fd = memfd();
    let mut lock = TestUserValue::from_value(flock(F_UNLCK, i64::MAX, 2, 0)).unwrap();
    let arg = lock.as_user_ptr() as usize;
    assert_eq!(sys_fcntl(fd, F_GETLK as _, arg), Err(KError::InvalidInput));
    assert_eq!(
        sys_fcntl(fd, F_OFD_GETLK as _, arg),
        Err(KError::from(LinuxError::EOVERFLOW))
    );
    lock.write(flock(F_WRLCK, 12, 34, -1));
    assert_eq!(
        sys_fcntl(fd, F_OFD_GETLK as _, arg),
        Err(KError::InvalidInput)
    );
    assert_eq!(sys_fcntl(fd, F_GETLK as _, arg), Ok(0));
    let result = lock.read();
    assert_eq!(result.l_type as u32, F_UNLCK);
    assert_eq!((result.l_start, result.l_len, result.l_pid), (12, 34, -1));
    assert_eq!(result.l_whence as u32, SEEK_SET);
    sys_close(fd).unwrap();
}

#[def_test(user)]
fn ofd_query_and_duplicate_close_lifetimes() {
    let fd = memfd();
    let duplicate = sys_dup(fd).unwrap() as i32;
    let mut lock = TestUserValue::from_value(flock(F_WRLCK, 10, 20, 0)).unwrap();
    let arg = lock.as_user_ptr() as usize;
    assert_eq!(sys_fcntl(fd, F_OFD_SETLK as _, arg), Ok(0));
    sys_close(fd).unwrap();
    lock.write(flock(F_UNLCK, 0, 0, 0));
    assert_eq!(sys_fcntl(duplicate, F_OFD_GETLK as _, arg), Ok(0));
    let result = lock.read();
    assert_eq!(result.l_type as u32, F_WRLCK);
    assert_eq!((result.l_start, result.l_len, result.l_pid), (10, 20, -1));
    lock.write(flock(F_UNLCK, 0, 0, 0));
    assert_eq!(sys_fcntl(duplicate, F_OFD_SETLK as _, arg), Ok(0));
    lock.write(flock(F_WRLCK, 10, 20, 0));
    assert_eq!(sys_fcntl(duplicate, F_SETLK as _, arg), Ok(0));
    let other = sys_dup(duplicate).unwrap() as i32;
    sys_close(other).unwrap();
    // Closing any fd drops the table's POSIX locks, even with another fd open.
    lock.write(flock(F_WRLCK, 0, 0, 0));
    assert_eq!(sys_fcntl(duplicate, F_OFD_GETLK as _, arg), Ok(0));
    assert_eq!(lock.read().l_type as u32, F_UNLCK);
    sys_close(duplicate).unwrap();
}

#[def_test(user)]
fn blocked_posix_lock_retries_after_files_replacement() {
    use alloc::sync::Arc;
    use core::sync::atomic::{AtomicBool, Ordering};

    use ktask::TaskState;
    use kvfs::{FileLock, FileLockType};

    let fd = memfd();
    let file = kprocess::current_resources().get_file(fd).unwrap();
    let resources = kprocess::current_resources();
    let mut lock = TestUserValue::from_value(flock(F_WRLCK, 0, 0, 0)).unwrap();
    let arg = lock.as_user_ptr() as usize;
    sys_fcntl(fd, F_OFD_SETLK as _, arg).unwrap();
    let waiter = ktask::current().clone();
    let observed_wait = Arc::new(AtomicBool::new(false));
    let worker = ktask::spawn({
        let observed_wait = observed_wait.clone();
        move || {
            // The held OFD lock prevents completion until this worker unlocks.
            // Bound failure and still release it so a regression cannot hang QEMU.
            for _ in 0..100000 {
                if waiter.state() == TaskState::Blocked {
                    observed_wait.store(true, Ordering::Release);
                    break;
                }
                ktask::yield_now();
            }
            resources.unshare_fd_table().unwrap();
            resources.close_range(fd + 1, i32::MAX).unwrap();
            file.set_lock(
                &FileLock::new(
                    kvfs::FileLockOwner::ofd(&file),
                    0,
                    FileLockType::Unlock,
                    0,
                    0,
                )
                .unwrap(),
                false,
            )
            .unwrap();
        }
    });
    assert_eq!(sys_fcntl(fd, F_SETLKW as _, arg), Ok(0));
    assert_eq!(worker.join(), 0);
    assert!(observed_wait.load(Ordering::Acquire));
    lock.write(flock(F_WRLCK, 0, 0, 0));
    sys_fcntl(fd, F_OFD_GETLK as _, arg).unwrap();
    assert_eq!(lock.read().l_type as u32, F_WRLCK);
    sys_close(fd).unwrap();
}
