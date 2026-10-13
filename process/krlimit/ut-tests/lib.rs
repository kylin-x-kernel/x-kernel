// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_rlimit_new() {
    let r = Rlimit::new(1, 2);
    assert_eq!(r.current, 1);
    assert_eq!(r.max, 2);
}

#[def_test]
fn test_rlimit_from() {
    let r: Rlimit = 3_u64.into();
    assert_eq!(r.current, 3);
    assert_eq!(r.max, 3);
}

#[def_test]
fn test_rlimits_new() {
    let limits = Rlimits::new(0x80000);
    assert_eq!(limits[RLIMIT_STACK].current, 0x80000);
    assert_eq!(limits[RLIMIT_NOFILE].current, FILE_LIMIT as u64);
    assert_eq!(limits[linux_raw_sys::general::RLIMIT_CPU], Rlimit::INFINITY);
    assert_eq!(limits[RLIMIT_CORE], Rlimit::new(0, RLIM_INFINITY));
    assert_eq!(limits[RLIMIT_NPROC], Rlimit::INFINITY);
}

#[def_test]
fn test_rlimits_index_mut_updates_selected_limit() {
    let mut limits = Rlimits::new(0x80000);
    limits[RLIMIT_NOFILE] = Rlimit::new(128, 256);

    assert_eq!(limits[RLIMIT_NOFILE].current, 128);
    assert_eq!(limits[RLIMIT_NOFILE].max, 256);
    assert_eq!(limits[RLIMIT_STACK].current, 0x80000);
}

#[def_test]
fn test_rlimits_preserve_defaults() {
    let limits = Rlimits::new(0x80000);
    assert_eq!(limits[RLIMIT_STACK].max, 0x80000);
    assert_eq!(limits[RLIMIT_NOFILE].max, FILE_LIMIT as u64);
    assert_eq!(
        limits[RLIMIT_MEMLOCK],
        Rlimit::new(MLOCK_LIMIT_BYTES, MLOCK_LIMIT_BYTES)
    );
    assert_eq!(
        limits[RLIMIT_MSGQUEUE],
        Rlimit::new(MSGQUEUE_LIMIT_BYTES, MSGQUEUE_LIMIT_BYTES)
    );
}
