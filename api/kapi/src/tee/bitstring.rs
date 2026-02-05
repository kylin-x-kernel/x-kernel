// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

/// 位图类型别名
pub type BitStr = u8;

/// 计算位所在的字节索引
#[inline]
fn bit_byte(bit: usize) -> usize {
    bit >> 3
}

/// 计算位在字节内的掩码
#[inline]
fn bit_mask(bit: usize) -> u8 {
    1 << (bit & 0x7)
}

/// 计算 nbits 位所需的字节数
#[inline]
pub fn bitstr_size(nbits: usize) -> usize {
    (nbits + 7) >> 3
}

#[inline]
pub fn bit_test(name: &[u8], bit: usize) -> bool {
    (name[bit_byte(bit)] & bit_mask(bit)) != 0
}

#[inline]
pub fn bit_set(name: &mut [u8], bit: usize) {
    name[bit_byte(bit)] |= bit_mask(bit);
}

#[inline]
pub fn bit_clear(name: &mut [u8], bit: usize) {
    name[bit_byte(bit)] &= !bit_mask(bit);
}

pub fn bit_nclear(name: &mut [u8], start: usize, stop: usize) {
    let start_byte = bit_byte(start);
    let stop_byte = bit_byte(stop);
    if start_byte == stop_byte {
        name[start_byte] &= (0xff >> (8 - (start & 0x7))) | (0xff << ((stop & 0x7) + 1));
    } else {
        name[start_byte] &= 0xff >> (8 - (start & 0x7));

        name[(start_byte + 1)..stop_byte].fill(0);

        name[stop_byte] &= 0xff << ((stop & 0x7) + 1);
    }
}

pub fn bit_ffc(name: &[u8], nbits: usize, value: &mut isize) {
    let stop_byte = bit_byte(nbits - 1);
    let mut val: isize = -1;
    if nbits > 0 {
        for (byte_index, &byte) in name.iter().enumerate().take(stop_byte + 1) {
            if byte != 0xff {
                let mut lb = byte;
                val = (byte_index << 3) as isize;
                while (lb & 0x1) != 0 {
                    val += 1;
                    lb >>= 1;
                }
                break;
            }
        }
    }
    if val as usize >= nbits {
        val = -1;
    }
    *value = val;
}

#[cfg(feature = "tee_test")]
pub mod tests_bitstring {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;

    test_fn! {
        using TestResult;

        fn test_bit_ffc() {
            let mut val: isize;

            // case 1: 全 0 => 第 0 位清除
            let bits = [0x00];
            val = -2;
            bit_ffc(&bits, 8, &mut val);
            assert_eq!(val, 0);

            // case 2: 00000001b => 第 1 位清除
            let bits = [0x01];
            val = -2;
            bit_ffc(&bits, 8, &mut val);
            assert_eq!(val, 1);

            // case 3: 11111111b => 全部 1 => 没有清除位
            let bits = [0xff];
            val = -2;
            bit_ffc(&bits, 8, &mut val);
            assert_eq!(val, -1);

            // case 4: 跨字节查找
            // byte0 = 0xff (全 1), byte1 = 0b11110111 (bit[11]=0)
            let bits = [0xff, 0b11110111];
            val = -2;
            bit_ffc(&bits, 16, &mut val);
            assert_eq!(val, 11);

            // case 5: 越界限制 — nbits < 实际位数
            let bits = [0x7f]; // 01111111 => bit[7]=0
            val = -2;
            bit_ffc(&bits, 7, &mut val); // 只检查前 7 位（0..6）
            assert_eq!(val, -1);
        }
    }

    tests_name! {
        TEST_BITSTRING;
        //------------------------
        bitstring;
        test_bit_ffc
    }
}
