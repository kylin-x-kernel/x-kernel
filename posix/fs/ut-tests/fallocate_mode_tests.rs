// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Entry-level fallocate flag validation and range guards.
//!
//! These cover flag decoding plus `validate`-style legality checks and the
//! `sys_fallocate` argument guards, so the errno mapping (EOPNOTSUPP for mode
//! problems, EINVAL for range problems) cannot silently drift.

use super::*;

fn assert_eopnotsupp<T: core::fmt::Debug>(result: KResult<T>) {
    assert_eq!(LinuxError::from(result.unwrap_err()), LinuxError::EOPNOTSUPP);
}

fn assert_einval<T: core::fmt::Debug>(result: KResult<T>) {
    assert_eq!(LinuxError::from(result.unwrap_err()), LinuxError::EINVAL);
}

#[unittest::def_test]
fn unknown_bits_are_rejected() {
    // Assertions live at the decode level: ksys_fallocate resolves the
    // descriptor before anything else, so a full-entry call needs a real
    // open descriptor, which the unit environment does not provide.
    assert_eopnotsupp(parse_fallocate(0x8000_0000));
    // FALLOC_FL_NO_HIDE_STALE (0x04) is not in the VFS-level supported mask
    // (include/linux/falloc.h:28), so it is not a decodable flag here.
    assert_eopnotsupp(parse_fallocate(0x04));
    assert_eopnotsupp(parse_fallocate((FallocateFlags::PUNCH_HOLE | FallocateFlags::ZERO_RANGE).bits()));
    assert_eopnotsupp(parse_fallocate(FallocateFlags::PUNCH_HOLE.bits()));
}

#[unittest::def_test]
fn punch_zero_and_missing_keep_size_are_eopnotsupp() {
    let punch = FallocateFlags::PUNCH_HOLE;
    let zero = FallocateFlags::ZERO_RANGE;
    let keep = FallocateFlags::KEEP_SIZE;
    assert_eopnotsupp(parse_fallocate((punch | zero).bits()));
    assert_eopnotsupp(parse_fallocate((punch | zero | keep).bits()));
    assert_eopnotsupp(parse_fallocate(punch.bits()));
}

#[unittest::def_test]
fn exclusive_range_modes_reject_extra_bits() {
    let zero = FallocateFlags::ZERO_RANGE;
    let keep = FallocateFlags::KEEP_SIZE;
    assert_einval(parse_fallocate(
        (FallocateFlags::COLLAPSE_RANGE | keep).bits(),
    ));
    assert_einval(parse_fallocate((FallocateFlags::INSERT_RANGE | zero).bits()));
    // UNSHARE|PUNCH would fail earlier as punch-without-KEEP_SIZE
    // (EOPNOTSUPP, same order as vfs_fallocate), so pair UNSHARE with ZERO.
    assert_einval(parse_fallocate((FallocateFlags::UNSHARE_RANGE | zero).bits()));
}

#[unittest::def_test]
fn valid_modes_pass_validation() {
    let modes = [
        FallocateFlags::empty(),
        FallocateFlags::KEEP_SIZE,
        FallocateFlags::PUNCH_HOLE | FallocateFlags::KEEP_SIZE,
        FallocateFlags::ZERO_RANGE,
        FallocateFlags::ZERO_RANGE | FallocateFlags::KEEP_SIZE,
        FallocateFlags::COLLAPSE_RANGE,
        FallocateFlags::INSERT_RANGE,
        FallocateFlags::UNSHARE_RANGE,
        FallocateFlags::UNSHARE_RANGE | FallocateFlags::KEEP_SIZE,
    ];
    for mode in modes {
        parse_fallocate(mode.bits()).expect("mode should pass validation");
    }
}

// The range guards (`offset < 0 || len <= 0` -> EINVAL) sit immediately
// after descriptor resolution and are covered by the user-mode cases at the
// bottom of this file. Mode legality is covered by the decode-level cases
// above: a full-entry mode assertion cannot observe EOPNOTSUPP without a
// valid descriptor, because resolution answers EBADF first.

#[unittest::def_test(user)]
fn fallocate_resolves_the_descriptor_first() {
    // ksys_fallocate's fdget precedes every vfs_fallocate check: an invalid
    // descriptor wins over an illegal range and an illegal mode alike
    // (EBADF, never EINVAL or EOPNOTSUPP).
    assert_eq!(
        LinuxError::from(sys_fallocate(-1, 0, -1, 1).unwrap_err()),
        LinuxError::EBADF
    );
    assert_eq!(
        LinuxError::from(sys_fallocate(-1, 0x8000_0000, 1, 1).unwrap_err()),
        LinuxError::EBADF
    );
}

#[unittest::def_test(user)]
fn fallocate_range_guards_answer_einval_on_real_descriptors() {
    use core::ffi::c_char;
    use linux_raw_sys::general::{O_CREAT, O_RDWR, AT_FDCWD};
    use posix_types::UserConstPtr;
    use unittest_support::TestUserArray;

    // The range guards run before mode legality and write permission, so a
    // freshly created descriptor is enough to reach them; the mode tests
    // above cover what follows.
    let path = TestUserArray::from_array(*b"/tmp/fallocate-guard-probe\0").unwrap();
    let fd = crate::open::sys_openat(
        AT_FDCWD,
        UserConstPtr::from(path.as_user_ptr().cast::<c_char>().cast_const()),
        (O_CREAT | O_RDWR) as i32,
        0o600,
    )
    .expect("open fallocate guard probe file") as c_int;
    assert_eq!(
        LinuxError::from(sys_fallocate(fd, 0, -1, 1).unwrap_err()),
        LinuxError::EINVAL
    );
    assert_eq!(
        LinuxError::from(sys_fallocate(fd, 0, 1, 0).unwrap_err()),
        LinuxError::EINVAL
    );
}
