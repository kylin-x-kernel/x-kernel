// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use super::*;

#[test]
fn llvm_22_size_helpers_round_partial_entries_up() {
    let data = core::mem::MaybeUninit::<[LlvmProfileData; 3]>::uninit();
    let data_begin = data.as_ptr().cast::<LlvmProfileData>();
    let data_end = data_begin.wrapping_add(3);
    let partial_data_end = data_begin
        .cast::<u8>()
        .wrapping_add(65)
        .cast::<LlvmProfileData>();
    let counters = [0u8; 13];
    let counters_begin = counters.as_ptr();
    let counters_end = counters_begin.wrapping_add(counters.len());

    assert_eq!(core::mem::size_of::<LlvmProfileData>(), 64);
    assert_eq!(__llvm_profile_get_num_data(data_begin, data_begin), 0);
    assert_eq!(__llvm_profile_get_data_size(data_begin, data_begin), 0);
    assert_eq!(__llvm_profile_get_num_data(data_begin, data_end), 3);
    assert_eq!(
        __llvm_profile_get_data_size(data_begin, data_end),
        (3 * core::mem::size_of::<LlvmProfileData>()) as u64,
    );
    assert_eq!(__llvm_profile_get_num_data(data_begin, partial_data_end), 2);
    assert_eq!(
        __llvm_profile_get_data_size(data_begin, partial_data_end),
        128
    );
    assert_eq!(
        __llvm_profile_get_counters_size(counters_begin, counters_begin),
        0
    );
    assert_eq!(
        __llvm_profile_get_counters_size(counters_begin, counters_end),
        16,
    );
    assert_eq!(
        core::mem::size_of_val(&__llvm_profile_runtime),
        core::mem::size_of::<i32>()
    );
}
