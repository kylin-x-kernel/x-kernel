// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Timestamp source of the "set to now" requests issued by utime(2),
//! utimes(2) and utimensat(2).

use core::ffi::{c_char, c_int, c_long};

use ktime_types::SystemTime;
use linux_raw_sys::general::*;
use posix_types::UserConstPtr;
use unittest::{assert, assert_eq, def_test};
use unittest_support::TestUserArray;

/// Returns the resident `(mtime, ctime)` of the open file.
fn file_times(fd: c_int) -> (SystemTime, SystemTime) {
    let metadata = crate::path::resolve_at(fd, None, AT_EMPTY_PATH)
        .unwrap()
        .into_path()
        .unwrap()
        .getattr()
        .unwrap();
    (metadata.mtime, metadata.ctime)
}

#[def_test(user)]
fn utimensat_utime_now_samples_the_tick_granular_clock() {
    let path = TestUserArray::from_array(*b"/tmp/ut-utime-now\0").unwrap();
    let name = UserConstPtr::from(path.as_user_ptr().cast::<c_char>().cast_const());
    let _ = crate::sys_unlinkat(AT_FDCWD, name, 0);
    let fd = crate::sys_openat(AT_FDCWD, name, (O_CREAT | O_RDWR) as i32, 0o600).unwrap() as c_int;

    let coarse_before = ktime::realtime_coarse();
    let times = TestUserArray::from_array([
        timespec {
            tv_sec: 0,
            tv_nsec: UTIME_NOW as c_long,
        },
        timespec {
            tv_sec: 0,
            tv_nsec: UTIME_NOW as c_long,
        },
    ])
    .unwrap();
    assert_eq!(
        crate::sys_utimensat(
            AT_FDCWD,
            name,
            UserConstPtr::from(times.as_user_ptr().cast::<[timespec; 2]>().cast_const()),
            0
        ),
        Ok(0)
    );
    let coarse_after = ktime::realtime_coarse();
    let (mtime, ctime) = file_times(fd);

    // "Set to now" must publish the tick-granular value. When both coarse
    // samples land in the same tick the result must equal that tick's value;
    // a fine-grained source would land strictly after it. A tick boundary
    // crossed inside the call widens the accepted window to [before, after].
    if coarse_before == coarse_after {
        assert_eq!(mtime, coarse_before);
    } else {
        assert!(mtime >= coarse_before);
        assert!(mtime <= coarse_after);
    }
    assert_eq!(ctime, mtime);

    // A following buffered write publishes the same clock again, so it cannot
    // move the timestamps backwards relative to the request above.
    let data = TestUserArray::from_array(*b"x").unwrap();
    assert_eq!(
        crate::sys_write(fd, UserConstPtr::from(data.as_user_ptr().cast_const()), 1),
        Ok(1)
    );
    let (mtime_after_write, ctime_after_write) = file_times(fd);
    assert!(mtime_after_write >= mtime);
    assert!(mtime_after_write <= ktime::realtime_coarse());
    assert_eq!(ctime_after_write, mtime_after_write);

    crate::sys_close(fd).unwrap();
    crate::sys_unlinkat(AT_FDCWD, name, 0).unwrap();
}
