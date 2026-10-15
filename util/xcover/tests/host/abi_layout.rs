// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use core::mem::{align_of, size_of};

use super::*;

/// Required alignment for `__llvm_profile_data`.
const INSTR_PROF_DATA_ALIGNMENT: usize = 8;

#[test]
fn llvm_profile_data_layout() {
    #[cfg(target_pointer_width = "64")]
    assert_eq!(size_of::<LlvmProfileData>(), 64);
    assert_eq!(align_of::<LlvmProfileData>(), INSTR_PROF_DATA_ALIGNMENT);
}

#[test]
fn llvm_profile_header_layout() {
    assert_eq!(size_of::<LlvmProfileHeader>(), 128);
    assert_eq!(align_of::<LlvmProfileHeader>(), 8);
}

#[test]
fn value_prof_node_layout() {
    assert_eq!(size_of::<ValueProfNode>(), 24);
    assert_eq!(align_of::<ValueProfNode>(), 8);
}

#[test]
fn padding_calculation() {
    assert_eq!(get_num_padding_bytes(0), 0);
    assert_eq!(get_num_padding_bytes(8), 0);
    assert_eq!(get_num_padding_bytes(1), 7);
    assert_eq!(get_num_padding_bytes(9), 7);
    assert_eq!(get_num_padding_bytes(16), 0);
}

#[test]
fn magic_numbers() {
    assert_eq!(INSTR_PROF_RAW_MAGIC_64 >> 56, 0xFF);
    assert_eq!(INSTR_PROF_RAW_MAGIC_32 >> 56, 0xFF);
}
