// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Wire-string formatting shared by the mode builders.

/// Writes the decimal form of `value` at `cursor`, returning the new cursor.
///
/// Mode names live in a fixed 32-byte array and the builders run on the ioctl
/// path, so the conversion must not allocate.
pub(crate) fn write_decimal(out: &mut [u8], mut cursor: usize, value: u32) -> usize {
    let mut digits = [0u8; 10];
    let mut len = 0;
    let mut rest = value;
    loop {
        digits[len] = b'0' + (rest % 10) as u8;
        len += 1;
        rest /= 10;
        if rest == 0 {
            break;
        }
    }
    while len > 0 {
        len -= 1;
        if cursor < out.len() {
            out[cursor] = digits[len];
            cursor += 1;
        }
    }
    cursor
}
