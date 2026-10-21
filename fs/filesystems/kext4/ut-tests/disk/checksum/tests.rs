// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::{bitmap_checksum, crc32c};

#[def_test]
fn linux_crc32c_golden_vector() {
    assert_eq!(crc32c(u32::MAX, b"123456789"), 0x1cf9_6d7c);
}

#[def_test]
fn bitmap_checksum_matches_linux_seeded_bitmap_crc() {
    let seed = 0x1122_3344;
    let bitmap = [0b1010_0101, 0x5a, 0xff, 0x00];

    assert_eq!(bitmap_checksum(&bitmap, seed), crc32c(seed, &bitmap));
}
