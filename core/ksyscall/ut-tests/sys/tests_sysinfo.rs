// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::vec;
use core::mem::{offset_of, size_of};

use ktime_types::TimeSpan;
use linux_raw_sys::general::{CLOCK_BOOTTIME, timespec};
use posix_types::{UserPtr, system::k_sysinfo};
use unittest::def_test;
use unittest_support::{TestUserBuffer, TestUserValue};

use super::{sys_sysinfo, sysinfo_uptime_seconds};

#[def_test]
fn test_sysinfo_uptime_rounding_and_saturation() {
    assert_eq!(sysinfo_uptime_seconds(TimeSpan::from_secs(0)), 0);
    assert_eq!(sysinfo_uptime_seconds(TimeSpan::from_nanos(1)), 1);
    assert_eq!(sysinfo_uptime_seconds(TimeSpan::from_secs(2)), 2);
    assert_eq!(
        sysinfo_uptime_seconds(TimeSpan::from_nanos(2_000_000_001)),
        3
    );
    assert_eq!(
        sysinfo_uptime_seconds(TimeSpan::from_secs(u64::MAX)),
        isize::MAX
    );
}

#[def_test(user)]
fn test_sysinfo_uptime_matches_boot_clock() {
    let size = size_of::<k_sysinfo>();
    let buf = TestUserBuffer::new(size).unwrap();
    let clock = TestUserValue::from_value(timespec {
        tv_sec: 0,
        tv_nsec: 0,
    })
    .unwrap();
    crate::time::sys_clock_gettime(CLOCK_BOOTTIME as i32, UserPtr::from(clock.as_user_ptr()))
        .unwrap();
    let before = clock.read();
    let ptr = UserPtr::from(buf.as_user_ptr::<k_sysinfo>());
    sys_sysinfo(ptr).unwrap();
    let bytes = buf.read_bytes(size).unwrap();
    let uptime = i64::from_ne_bytes(bytes[..8].try_into().unwrap());
    crate::time::sys_clock_gettime(CLOCK_BOOTTIME as i32, UserPtr::from(clock.as_user_ptr()))
        .unwrap();
    let after = clock.read();
    // Compare independent ABI reads, not the conversion helper under test.
    let before_rounded = before.tv_sec + i64::from(before.tv_nsec != 0);
    let after_rounded = after.tv_sec + i64::from(after.tv_nsec != 0);
    assert!(uptime >= before_rounded && uptime <= after_rounded);
}

#[def_test(user)]
fn test_sysinfo_zeroes_abi_padding() {
    let size = size_of::<k_sysinfo>();
    let buf = TestUserBuffer::new(size).unwrap();
    buf.write_bytes(&vec![0x5a; size]).unwrap();

    sys_sysinfo(UserPtr::from(buf.as_user_ptr::<k_sysinfo>())).unwrap();

    let bytes = buf.read_bytes(size).unwrap();
    let gap = offset_of!(k_sysinfo, pad) + size_of::<u16>()..offset_of!(k_sysinfo, totalhigh);
    let tail = offset_of!(k_sysinfo, mem_unit) + size_of::<u32>()..size;
    assert!(bytes[gap].iter().all(|&byte| byte == 0));
    assert!(bytes[tail].iter().all(|&byte| byte == 0));
}
