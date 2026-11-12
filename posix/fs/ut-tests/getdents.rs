// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};
use unittest_support::TestUserArray;

use super::*;

#[def_test(user)]
fn getdents_large_capacity_writes_only_completed_records() {
    let path = TestUserArray::from_array(*b"/\0").unwrap();
    let output = TestUserArray::from_array([b'!'; 4096]).unwrap();
    let fd = crate::open::sys_openat(
        AT_FDCWD,
        UserConstPtr::from(path.as_user_ptr().cast::<c_char>().cast_const()),
        (O_RDONLY | O_DIRECTORY) as i32,
        0,
    )
    .unwrap() as i32;
    let buf = UserPtr::from(output.as_user_ptr());
    assert_eq!(sys_getdents64(-1, UserPtr::default(), u32::MAX), Err(KError::BadFileDescriptor));
    assert_eq!(sys_getdents64(fd, buf, 1), Err(KError::InvalidInput));
    let position = kprocess::current_files().get_file(fd).unwrap().position();
    assert_eq!(sys_getdents64(fd, UserPtr::default(), u32::MAX), Err(KError::BadAddress));
    assert_eq!(kprocess::current_files().get_file(fd).unwrap().position(), position);

    // Only the real directory records need mapped output, regardless of the
    // advertised multi-gigabyte capacity. The old temporary Vec panicked here.
    let count = sys_getdents64(fd, buf, i32::MAX as u32).unwrap() as usize;
    assert!(count > 0 && count <= 4096);
    let bytes = output.read();
    let mut offset = 0;
    while offset < count {
        let reclen = u16::from_ne_bytes([bytes[offset + 16], bytes[offset + 17]]) as usize;
        assert!(reclen >= 24 && reclen.is_multiple_of(8) && offset + reclen <= count);
        assert!(bytes[offset + 19..offset + reclen].contains(&0));
        offset += reclen;
    }
    assert_eq!(offset, count);
    while sys_getdents64(fd, buf, 4096).unwrap() != 0 {}
    assert_eq!(sys_getdents64(fd, UserPtr::default(), u32::MAX), Ok(0));
    crate::fd_ops::sys_close(fd).unwrap();
}
