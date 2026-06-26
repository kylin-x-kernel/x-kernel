// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LLVM profiling data structures.
//!
//! Defines the binary layout of `.profraw` profile data.
//! All structures use `#[repr(C)]` to match the LLVM compiler-rt ABI.
//! These types are `pub(crate)` — they must not leak outside the `abi` module.

use core::ffi::c_void;

// === Constants from InstrProfData.inc ===

/// Raw profile version number.
pub(crate) const INSTR_PROF_RAW_VERSION: u64 = 10;

// === Core data structures ===

/// Magic number for 64-bit raw profile format.
/// Encodes "lprofr" in bytes 1-6: 0xFF 'l' 'p' 'r' 'o' 'f' 'r' 0x81
pub(crate) const INSTR_PROF_RAW_MAGIC_64: u64 = (255u64 << 56)
    | ((b'l' as u64) << 48)
    | ((b'p' as u64) << 40)
    | ((b'r' as u64) << 32)
    | ((b'o' as u64) << 24)
    | ((b'f' as u64) << 16)
    | ((b'r' as u64) << 8)
    | 129u64;

/// Magic number for 32-bit raw profile format.
/// Encodes "lprofR" in bytes 1-6: 0xFF 'l' 'p' 'r' 'o' 'f' 'R' 0x81
pub(crate) const INSTR_PROF_RAW_MAGIC_32: u64 = (255u64 << 56)
    | ((b'l' as u64) << 48)
    | ((b'p' as u64) << 40)
    | ((b'r' as u64) << 32)
    | ((b'o' as u64) << 24)
    | ((b'f' as u64) << 16)
    | ((b'R' as u64) << 8)
    | 129u64;

// Variant masks for profile versioning.
pub(crate) const VARIANT_MASKS_ALL: u64 = 0xffff_ffff_0000_0000;
pub(crate) const VARIANT_MASK_BYTE_COVERAGE: u64 = 1u64 << 60;

// Value profiling kinds.
pub(crate) const IPVK_INDIRECT_CALL_TARGET: u32 = 0;
pub(crate) const IPVK_VTABLE_TARGET: u32 = 2;
pub(crate) const IPVK_FIRST: u32 = IPVK_INDIRECT_CALL_TARGET;
pub(crate) const IPVK_LAST: u32 = IPVK_VTABLE_TARGET;

/// Default number of values per value profiling site.
pub(crate) const INSTR_PROF_DEFAULT_NUM_VAL_PER_SITE: u32 = 24;

/// Number of value profiling kinds.
pub(crate) const IPVK_NUM_KINDS: usize = (IPVK_LAST - IPVK_FIRST + 1) as usize;

// === Core data structures ===

/// Per-function control structure placed in the `__llvm_prf_data` section.
#[repr(C)]
#[derive(Clone, Copy, Debug)]
pub(crate) struct LlvmProfileData {
    pub name_ref: u64,
    pub func_hash: u64,
    pub counter_ptr: *mut c_void,
    pub bitmap_ptr: *mut c_void,
    pub function_pointer: *mut c_void,
    pub values: *mut c_void,
    pub num_counters: u32,
    pub num_value_sites: [u16; IPVK_NUM_KINDS],
    pub num_bitmap_bytes: u32,
}

/// Header at the start of a raw profile (.profraw) buffer.
/// Only used for layout validation tests.
#[cfg(test)]
#[repr(C)]
#[derive(Clone, Copy, Debug)]
struct LlvmProfileHeader {
    magic: u64,
    version: u64,
    binary_ids_size: u64,
    num_data: u64,
    padding_bytes_before_counters: u64,
    num_counters: u64,
    padding_bytes_after_counters: u64,
    num_bitmap_bytes: u64,
    padding_bytes_after_bitmap_bytes: u64,
    names_size: u64,
    counters_delta: u64,
    bitmap_delta: u64,
    names_delta: u64,
    num_vtables: u64,
    vnames_size: u64,
    value_kind_last: u64,
}

/// Node in the value profiling linked list.
#[repr(C)]
#[derive(Debug)]
pub(crate) struct ValueProfNode {
    pub value: u64,
    pub count: u64,
    pub next: *mut ValueProfNode,
}

/// Returns the number of padding bytes needed to align `size_bytes` to 8 bytes.
pub(crate) const fn get_num_padding_bytes(size_bytes: u64) -> u8 {
    (7 & (8 - size_bytes % 8)) as u8
}

// SAFETY: These types contain raw pointers that are only read by the
// profiling runtime. The runtime operates in single-threaded contexts.
unsafe impl Sync for LlvmProfileData {}
// SAFETY: `ValueProfNode` instances live in runtime-managed storage and are
// synchronized externally by the profiling runtime's update protocol.
unsafe impl Sync for ValueProfNode {}

#[cfg(test)]
mod tests {
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
}
