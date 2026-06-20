// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LLVM profiling data structures.
//!
//! Defines the binary layout of `.profraw` profile data.
//! All structures use `#[repr(C)]` to match the LLVM compiler-rt ABI.

use core::ffi::c_void;

// === Constants from InstrProfData.inc ===

/// Raw profile version number.
pub const INSTR_PROF_RAW_VERSION: u64 = 10;

/// Required alignment for `__llvm_profile_data`.
pub const INSTR_PROF_DATA_ALIGNMENT: usize = 8;

/// Magic number for 64-bit raw profile format.
/// Encodes "lprofr" in bytes 1-6: 0xFF 'l' 'p' 'r' 'o' 'f' 'r' 0x81
pub const INSTR_PROF_RAW_MAGIC_64: u64 = (255u64 << 56)
    | ((b'l' as u64) << 48)
    | ((b'p' as u64) << 40)
    | ((b'r' as u64) << 32)
    | ((b'o' as u64) << 24)
    | ((b'f' as u64) << 16)
    | ((b'r' as u64) << 8)
    | 129u64;

/// Magic number for 32-bit raw profile format.
/// Encodes "lprofR" in bytes 1-6: 0xFF 'l' 'p' 'r' 'o' 'f' 'R' 0x81
pub const INSTR_PROF_RAW_MAGIC_32: u64 = (255u64 << 56)
    | ((b'l' as u64) << 48)
    | ((b'p' as u64) << 40)
    | ((b'r' as u64) << 32)
    | ((b'o' as u64) << 24)
    | ((b'f' as u64) << 16)
    | ((b'R' as u64) << 8)
    | 129u64;

// Variant masks for profile versioning.
pub const VARIANT_MASKS_ALL: u64 = 0xffff_ffff_0000_0000;
pub const VARIANT_MASK_IR_PROF: u64 = 1u64 << 56;
pub const VARIANT_MASK_BYTE_COVERAGE: u64 = 1u64 << 60;
pub const VARIANT_MASK_TEMPORAL_PROF: u64 = 1u64 << 63;

// Value profiling kinds.
pub const IPVK_INDIRECT_CALL_TARGET: u32 = 0;
pub const IPVK_MEM_OP_SIZE: u32 = 1;
pub const IPVK_VTABLE_TARGET: u32 = 2;
pub const IPVK_FIRST: u32 = IPVK_INDIRECT_CALL_TARGET;
pub const IPVK_LAST: u32 = IPVK_VTABLE_TARGET;

/// Default number of values per value profiling site.
pub const INSTR_PROF_DEFAULT_NUM_VAL_PER_SITE: u32 = 24;

/// Number of value profiling kinds.
pub const IPVK_NUM_KINDS: usize = (IPVK_LAST - IPVK_FIRST + 1) as usize;

// === Core data structures ===

/// Per-function control structure placed in the `__llvm_prf_data` section.
#[repr(C)]
#[derive(Debug)]
pub struct LlvmProfileData {
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
#[repr(C)]
#[derive(Debug)]
pub struct LlvmProfileHeader {
    pub magic: u64,
    pub version: u64,
    pub binary_ids_size: u64,
    pub num_data: u64,
    pub padding_bytes_before_counters: u64,
    pub num_counters: u64,
    pub padding_bytes_after_counters: u64,
    pub num_bitmap_bytes: u64,
    pub padding_bytes_after_bitmap_bytes: u64,
    pub names_size: u64,
    pub counters_delta: u64,
    pub bitmap_delta: u64,
    pub names_delta: u64,
    pub num_vtables: u64,
    pub vnames_size: u64,
    pub value_kind_last: u64,
}

/// Node in the value profiling linked list.
#[repr(C)]
#[derive(Debug)]
pub struct ValueProfNode {
    pub value: u64,
    pub count: u64,
    pub next: *mut ValueProfNode,
}

/// Top-level value profiling data block for a function.
#[repr(C)]
#[derive(Debug)]
pub struct ValueProfData {
    pub total_size: u32,
    pub num_value_kinds: u32,
}

/// Record for one value profiling kind within a function.
#[repr(C)]
#[derive(Debug)]
pub struct ValueProfRecord {
    pub kind: u32,
    pub num_value_sites: u32,
}

/// Single observed value and its count.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default)]
pub struct InstrProfValueData {
    pub value: u64,
    pub count: u64,
}

// === Writer types ===

/// Scatter/gather I/O vector for profile data writes.
#[repr(C)]
pub struct ProfDataIOVec {
    pub data: *mut c_void,
    pub elm_size: usize,
    pub num_elm: usize,
    pub use_zero_padding: i32,
}

/// Callback-based profile data writer.
#[repr(C)]
pub struct ProfDataWriter {
    pub write_fn: unsafe extern "C" fn(
        this: *mut ProfDataWriter,
        io_vecs: *mut ProfDataIOVec,
        num_io_vecs: u32,
    ) -> u32,
    pub writer_ctx: *mut c_void,
}

/// Buffered I/O wrapper over a `ProfDataWriter`.
#[repr(C)]
pub struct ProfBufferIO {
    pub file_writer: *mut ProfDataWriter,
    pub own_file_writer: u32,
    pub buffer_start: *mut u8,
    pub buffer_sz: u32,
    pub cur_offset: u32,
}

/// Trait object for reading value profiling data during serialization.
#[repr(C)]
pub struct VPDataReaderType {
    pub init_rt_record:
        unsafe extern "C" fn(data: *const LlvmProfileData, site_count_array: *mut *mut u8) -> u32,
    pub get_value_prof_record_header_size: unsafe extern "C" fn(num_sites: u32) -> u32,
    pub get_first_value_prof_record:
        unsafe extern "C" fn(data: *mut ValueProfData) -> *mut ValueProfRecord,
    pub get_num_value_data_for_site: unsafe extern "C" fn(value_kind: u32, site: u32) -> u32,
    pub get_value_prof_data_size: unsafe extern "C" fn() -> u32,
    pub get_value_data: unsafe extern "C" fn(
        value_kind: u32,
        site: u32,
        dst: *mut InstrProfValueData,
        start_node: *mut ValueProfNode,
        n: u32,
    ) -> *mut ValueProfNode,
}

/// Returns the number of padding bytes needed to align `size_bytes` to 8 bytes.
pub const fn get_num_padding_bytes(size_bytes: u64) -> u8 {
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

    #[test]
    fn llvm_profile_data_layout() {
        // Size depends on pointer width: 64 on 64-bit, smaller on 32-bit.
        // On 64-bit: 6×8 (ptr/u64) + 4 (u32) + 6 ([u16;3]) + 4 (u32) = 62 → padded to 64
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<LlvmProfileData>(), 64);
        assert_eq!(align_of::<LlvmProfileData>(), INSTR_PROF_DATA_ALIGNMENT);
    }

    #[test]
    fn llvm_profile_header_layout() {
        // 16 × u64 = 128 bytes
        assert_eq!(size_of::<LlvmProfileHeader>(), 128);
        assert_eq!(align_of::<LlvmProfileHeader>(), 8);
    }

    #[test]
    fn value_prof_node_layout() {
        assert_eq!(size_of::<ValueProfNode>(), 24);
        assert_eq!(align_of::<ValueProfNode>(), 8);
    }

    #[test]
    fn instr_prof_value_data_layout() {
        assert_eq!(size_of::<InstrProfValueData>(), 16);
        assert_eq!(align_of::<InstrProfValueData>(), 8);
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
