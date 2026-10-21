// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{
    ATIME_EXTRA_OFFSET, CTIME_EXTRA_OFFSET, EXTRA_ISIZE_OFFSET, MTIME_EXTRA_OFFSET, RawInode,
};

#[def_test]
fn decodes_owner_and_timestamp_fields() {
    let mut bytes = [0; 256];
    put_u16(&mut bytes, 0x02, 0x5678);
    put_u32(&mut bytes, 0x08, 11);
    put_u32(&mut bytes, 0x0c, 22);
    put_u32(&mut bytes, 0x10, 33);
    put_u32(&mut bytes, 0x14, 44);
    put_u16(&mut bytes, 0x18, 0xdef0);
    put_u32(&mut bytes, 0x68, 0x7654_3210);
    put_u16(&mut bytes, 0x70, 0xfedc);
    put_u16(&mut bytes, 0x78, 0x1234);
    put_u16(&mut bytes, 0x7a, 0x9abc);
    put_u16(&mut bytes, EXTRA_ISIZE_OFFSET, 16);
    put_u32(&mut bytes, CTIME_EXTRA_OFFSET, 0x0000_0001);
    put_u32(&mut bytes, MTIME_EXTRA_OFFSET, 0x0000_0002);
    put_u32(&mut bytes, ATIME_EXTRA_OFFSET, 0x0000_0003);

    let inode = RawInode::decode(&bytes).expect("decode inode metadata fields");

    assert_eq!(inode.uid(), 0x1234_5678);
    assert_eq!(inode.gid(), 0x9abc_def0);
    assert_eq!(inode.file_acl(), 0xfedc_7654_3210);
    assert_eq!(inode.atime(), 11);
    assert_eq!(inode.ctime(), 22);
    assert_eq!(inode.mtime(), 33);
    assert_eq!(inode.dtime(), 44);
    assert_eq!(inode.atime_extra(), Some(0x0000_0003));
    assert_eq!(inode.ctime_extra(), Some(0x0000_0001));
    assert_eq!(inode.mtime_extra(), Some(0x0000_0002));
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}
