// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use alloc::{format, string::String};

#[inline]
pub const fn bit32(nr: u32) -> u32 {
    1u32 << nr
}

#[inline]
pub const fn bit64(nr: u32) -> u64 {
    1u64 << nr
}

#[inline]
pub const fn bit(nr: u32) -> u32 {
    bit32(nr)
}

#[inline]
pub(crate) fn shift_u32(v: u32, shift: u32) -> u32 {
    v << shift
}

#[inline]
pub(crate) fn shift_u64(v: u64, shift: u32) -> u64 {
    v << shift
}

#[macro_export]
macro_rules! container_of {
    ($ptr:expr, $type:ty, $member:ident) => {{
        let ptr = $ptr as *const _;
        (ptr as usize - core::mem::offset_of!($type, $member)) as *mut $type
    }};
}

#[macro_export]
macro_rules! member_size {
    ($type:ty, $member:ident) => {
        core::mem::offset_of!($type, $member)
    };
}

pub fn roundup_u<
    T: Copy
        + core::ops::Add<Output = T>
        + core::ops::Sub<Output = T>
        + core::ops::BitAnd<Output = T>
        + core::ops::Not<Output = T>
        + From<u8>,
>(
    v: T,
    size: T,
) -> T {
    (v + (size - T::from(1))) & !(size - T::from(1))
}

pub fn slice_fmt(data: &[u8]) -> String {
    let min_len: usize = 2048;
    let len = data.len();
    let show_len = len.min(min_len);

    format!("len: 0x{:X}, ", len,)
        + "data: "
        + &hex::encode_upper(&data[..show_len])
        + if len > min_len { "..." } else { "" }
}

#[cfg(feature = "tee_test")]
pub mod tests_utils {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;

    test_fn! {
        using TestResult;

        fn test_slice_fmt() {
            let data = [0x12, 0x34, 0x56, 0x78];
            let result = slice_fmt(&data);
            assert_eq!(result, "len: 0x4, data: 12345678");
        }
    }

    tests_name! {
        TEST_TEE_UTILS;
        //------------------------
        utils;
        test_slice_fmt
    }
}
