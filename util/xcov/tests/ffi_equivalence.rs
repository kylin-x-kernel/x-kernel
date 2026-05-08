// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Comprehensive FFI boundary and API compatibility tests.
//!
//! Verifies xcov by checking:
//! 1. Struct layouts match LLVM compiler-rt ABI
//! 2. Constants match LLVM definitions
//! 3. Public Rust API surface matches xcov exactly
//! 4. All #[no_mangle] symbols are exported
//! 5. Behavioral equivalence for core operations

use std::mem::{align_of, size_of};

use xcov::CoverageWriter;

// =========================================================================
// 1. STRUCT LAYOUT TESTS — must match LLVM compiler-rt C ABI exactly
// =========================================================================

#[test]
fn struct_sizes_match() {
    #[cfg(target_pointer_width = "64")]
    assert_eq!(size_of::<xcov::types::LlvmProfileData>(), 64);
    assert_eq!(align_of::<xcov::types::LlvmProfileData>(), 8);
    assert_eq!(size_of::<xcov::types::LlvmProfileHeader>(), 128);
    assert_eq!(size_of::<xcov::types::ValueProfNode>(), 24);
    assert_eq!(size_of::<xcov::types::InstrProfValueData>(), 16);
}

#[test]
fn llvm_profile_data_field_offsets() {
    use xcov::types::LlvmProfileData;
    // All offsets verified against C struct compiled on aarch64 macOS.
    assert_eq!(std::mem::offset_of!(LlvmProfileData, name_ref), 0);
    assert_eq!(std::mem::offset_of!(LlvmProfileData, func_hash), 8);
    assert_eq!(std::mem::offset_of!(LlvmProfileData, counter_ptr), 16);
    assert_eq!(std::mem::offset_of!(LlvmProfileData, bitmap_ptr), 24);
    assert_eq!(std::mem::offset_of!(LlvmProfileData, function_pointer), 32);
    assert_eq!(std::mem::offset_of!(LlvmProfileData, values), 40);
    assert_eq!(std::mem::offset_of!(LlvmProfileData, num_counters), 48);
    assert_eq!(std::mem::offset_of!(LlvmProfileData, num_value_sites), 52);
    assert_eq!(std::mem::offset_of!(LlvmProfileData, num_bitmap_bytes), 60);
}

#[test]
fn llvm_profile_header_field_offsets() {
    use xcov::types::LlvmProfileHeader;
    // 16 × u64 fields, each 8 bytes apart.
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, magic), 0);
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, version), 8);
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, binary_ids_size), 16);
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, num_data), 24);
    assert_eq!(
        std::mem::offset_of!(LlvmProfileHeader, padding_bytes_before_counters),
        32
    );
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, num_counters), 40);
    assert_eq!(
        std::mem::offset_of!(LlvmProfileHeader, padding_bytes_after_counters),
        48
    );
    assert_eq!(
        std::mem::offset_of!(LlvmProfileHeader, num_bitmap_bytes),
        56
    );
    assert_eq!(
        std::mem::offset_of!(LlvmProfileHeader, padding_bytes_after_bitmap_bytes),
        64
    );
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, names_size), 72);
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, counters_delta), 80);
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, bitmap_delta), 88);
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, names_delta), 96);
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, num_vtables), 104);
    assert_eq!(std::mem::offset_of!(LlvmProfileHeader, vnames_size), 112);
    assert_eq!(
        std::mem::offset_of!(LlvmProfileHeader, value_kind_last),
        120
    );
}

#[test]
fn value_prof_node_field_offsets() {
    use xcov::types::ValueProfNode;
    assert_eq!(std::mem::offset_of!(ValueProfNode, value), 0);
    assert_eq!(std::mem::offset_of!(ValueProfNode, count), 8);
    assert_eq!(std::mem::offset_of!(ValueProfNode, next), 16);
}

#[test]
fn instr_prof_value_data_field_offsets() {
    use xcov::types::InstrProfValueData;
    assert_eq!(std::mem::offset_of!(InstrProfValueData, value), 0);
    assert_eq!(std::mem::offset_of!(InstrProfValueData, count), 8);
}

#[test]
fn writer_struct_sizes() {
    assert_eq!(size_of::<xcov::types::ProfDataWriter>(), 16);
    assert_eq!(size_of::<xcov::types::ProfDataIOVec>(), 32);
    assert_eq!(size_of::<xcov::types::ValueProfData>(), 8);
    assert_eq!(size_of::<xcov::types::ValueProfRecord>(), 8);
}

// =========================================================================
// 2. CONSTANT MATCHING TESTS — must match LLVM InstrProfData.inc
// =========================================================================

#[test]
fn constants_match_llvm() {
    assert_eq!(xcov::types::INSTR_PROF_RAW_VERSION, 10);
    assert_eq!(xcov::types::VARIANT_MASKS_ALL, 0xffff_ffff_0000_0000);
    assert_eq!(xcov::types::VARIANT_MASK_IR_PROF, 1u64 << 56);
    assert_eq!(xcov::types::VARIANT_MASK_BYTE_COVERAGE, 1u64 << 60);
    assert_eq!(xcov::types::VARIANT_MASK_TEMPORAL_PROF, 1u64 << 63);
    assert_eq!(xcov::types::IPVK_INDIRECT_CALL_TARGET, 0);
    assert_eq!(xcov::types::IPVK_MEM_OP_SIZE, 1);
    assert_eq!(xcov::types::IPVK_VTABLE_TARGET, 2);
    assert_eq!(xcov::types::IPVK_FIRST, 0);
    assert_eq!(xcov::types::IPVK_LAST, 2);
    assert_eq!(xcov::types::IPVK_NUM_KINDS, 3);
    assert_eq!(xcov::types::INSTR_PROF_DEFAULT_NUM_VAL_PER_SITE, 24);
    assert_eq!(xcov::types::INSTR_PROF_DATA_ALIGNMENT, 8);
}

#[test]
fn padding_matches_c_runtime() {
    assert_eq!(xcov::types::get_num_padding_bytes(0), 0);
    assert_eq!(xcov::types::get_num_padding_bytes(1), 7);
    assert_eq!(xcov::types::get_num_padding_bytes(7), 1);
    assert_eq!(xcov::types::get_num_padding_bytes(8), 0);
    assert_eq!(xcov::types::get_num_padding_bytes(9), 7);
    assert_eq!(xcov::types::get_num_padding_bytes(16), 0);
    assert_eq!(xcov::types::get_num_padding_bytes(17), 7);
    assert_eq!(xcov::types::get_num_padding_bytes(24), 0);
    assert_eq!(xcov::types::get_num_padding_bytes(100), 4);
}

#[test]
fn magic_number_properties() {
    // Both magic numbers must start with 0xFF in the high byte.
    assert_eq!(xcov::types::INSTR_PROF_RAW_MAGIC_64 >> 56, 0xFF);
    assert_eq!(xcov::types::INSTR_PROF_RAW_MAGIC_32 >> 56, 0xFF);
    // 64-bit and 32-bit magic differ at bit 32/33.
    assert_ne!(
        xcov::types::INSTR_PROF_RAW_MAGIC_64,
        xcov::types::INSTR_PROF_RAW_MAGIC_32
    );
}

#[test]
fn re_exported_constants_at_crate_root() {
    // minicov re-exports these at crate root; xcov must too.
    assert_eq!(xcov::INSTR_PROF_RAW_VERSION, 10);
    assert_eq!(xcov::VARIANT_MASKS_ALL, 0xffff_ffff_0000_0000);
}

// =========================================================================
// 3. PUBLIC RUST API SURFACE TESTS — verify every item minicov exposes
// =========================================================================

#[test]
fn public_trait_coverage_writer_exists() {
    // Verify the trait exists with the correct signature.
    fn _assert_trait<W: xcov::CoverageWriter>() {}
    fn _check() {
        _assert_trait::<Vec<u8>>();
    }
}

#[test]
fn coverage_writer_vec_impl() {
    let mut v: Vec<u8> = Vec::new();
    v.write(&[1, 2, 3]).unwrap();
    assert_eq!(v, vec![1, 2, 3]);
    v.write(&[4, 5]).unwrap();
    assert_eq!(v, vec![1, 2, 3, 4, 5]);
}

#[test]
fn error_types_exist_with_traits() {
    // CoverageWriteError: Copy + Clone + Debug + Display
    let e1 = xcov::CoverageWriteError;
    let _copy = e1;
    let _clone = e1.clone();
    let _debug = format!("{:?}", e1);
    let _display = format!("{}", e1);
    assert_eq!(_display, "error while writing coverage data");

    // IncompatibleCoverageData: Copy + Clone + Debug + Display
    let e2 = xcov::IncompatibleCoverageData;
    let _copy = e2;
    let _clone = e2.clone();
    let _debug = format!("{:?}", e2);
    let _display = format!("{}", e2);
    assert_eq!(_display, "incompatible coverage data");
}

#[test]
fn coverage_enabled_returns_bool() {
    // On macOS without -Cinstrument-coverage, this returns false.
    let _enabled: bool = xcov::coverage_enabled();
}

#[test]
fn module_signature_returns_u64() {
    let _sig: u64 = xcov::module_signature();
}

#[test]
fn reset_coverage_is_callable() {
    // reset_coverage is NOT unsafe in minicov, must not be in xcov either.
    xcov::reset_coverage();
}

#[test]
fn capture_coverage_signature() {
    // capture_coverage takes &mut Writer, returns Result.
    // Without actual instrumentation data, it returns Err on macOS.
    let mut buf: Vec<u8> = Vec::new();
    let result = xcov::capture_coverage(&mut buf);
    // On macOS without instrumentation, sections are empty so buffer is 0-size.
    // With empty sections, write_buffer may succeed with just a header,
    // or may fail. Either way the function type signature is verified.
    let _typed: Result<(), xcov::CoverageWriteError> = result;
}

#[test]
fn merge_coverage_signature() {
    // merge_coverage takes &[u8], returns Result.
    let data = [0u8; 64];
    let result = xcov::merge_coverage(&data);
    let _typed: Result<(), xcov::IncompatibleCoverageData> = result;
}

// =========================================================================
// 4. NO_MANGLE SYMBOL EXPORT TESTS
// =========================================================================

#[test]
fn static_exports_exist() {
    // These are #[no_mangle] statics that must exist as C-linkage symbols.
    let _val: u8 = xcov::__llvm_profile_runtime;
    let _ver: u64 = xcov::__llvm_profile_raw_version;
}

#[test]
fn section_accessor_exports() {
    unsafe {
        let _: *const u8 = xcov::__llvm_profile_begin_counters();
        let _: *const u8 = xcov::__llvm_profile_end_counters();
        let _: *const xcov::types::LlvmProfileData = xcov::__llvm_profile_begin_data();
        let _: *const xcov::types::LlvmProfileData = xcov::__llvm_profile_end_data();
        let _: *const u8 = xcov::__llvm_profile_begin_names();
        let _: *const u8 = xcov::__llvm_profile_end_names();
        let _: *const u8 = xcov::__llvm_profile_begin_bitmap();
        let _: *const u8 = xcov::__llvm_profile_end_bitmap();
        let _: *const xcov::types::ValueProfNode = xcov::__llvm_profile_begin_vnodes();
        let _: *const xcov::types::ValueProfNode = xcov::__llvm_profile_end_vnodes();
        let _: *const u8 = xcov::__llvm_profile_begin_vtables();
        let _: *const u8 = xcov::__llvm_profile_end_vtables();
        let _: *const u8 = xcov::__llvm_profile_begin_vtabnames();
        let _: *const u8 = xcov::__llvm_profile_end_vtabnames();
    }
}

#[test]
fn profiling_function_exports() {
    unsafe {
        xcov::__llvm_profile_reset_counters();
        let _: i32 = xcov::__llvm_profile_merge_from_buffer(std::ptr::null(), 0);
        let _: i32 = xcov::__llvm_profile_check_compatibility(std::ptr::null(), 0);
    }
    // __llvm_profile_write_buffer needs a valid buffer.
    let header_size = size_of::<xcov::types::LlvmProfileHeader>();
    let mut buf = vec![0u8; header_size * 2];
    unsafe {
        let result = xcov::__llvm_profile_write_buffer(buf.as_mut_ptr());
        assert_eq!(result, 0);
    }
    // These are safe extern "C" functions.
    let _: u64 = xcov::__llvm_profile_get_version();
    let _: u64 = xcov::__llvm_profile_get_magic();
    let _: u8 = xcov::__llvm_profile_get_num_padding_bytes(8);
    let _: u64 = xcov::__llvm_profile_get_size_for_buffer();
    let _: u64 = xcov::__llvm_profile_get_data_size();
    let _: u64 = xcov::__llvm_profile_get_counters_size();
    let _: u64 = xcov::__llvm_profile_get_num_data();
    let _: u64 = xcov::__llvm_profile_get_num_padding_bytes_for_counters();
}

#[test]
fn continuous_mode_exports() {
    assert_eq!(xcov::__llvm_profile_is_continuous_mode_enabled(), 0);
    xcov::__llvm_profile_enable_continuous_mode();
    xcov::__llvm_profile_disable_continuous_mode();
    xcov::__llvm_profile_set_page_size(4096);
    assert_eq!(xcov::__llvm_profile_is_continuous_mode_enabled(), 0);
}

#[test]
fn dumped_flag_exports() {
    xcov::__llvm_profile_set_dumped();
    xcov::__llvm_profile_initialize();
}

#[test]
fn allocator_hook_exports() {
    unsafe {
        let ptr = xcov::minicov_alloc_zeroed(64, 8);
        assert!(!ptr.is_null());
        xcov::minicov_dealloc(ptr, 64, 8);
    }
}

#[test]
fn lprof_exports() {
    let _: *mut xcov::types::VPDataReaderType = xcov::lprofGetVPDataReader();
    let _: u64 = xcov::lprofGetLoadModuleSignature();
}

#[test]
fn value_profiling_exports() {
    unsafe {
        xcov::__llvm_profile_instrument_target(0, std::ptr::null_mut(), 0);
        xcov::__llvm_profile_instrument_target_value(0, std::ptr::null_mut(), 0, 0);
        xcov::__llvm_profile_instrument_memop(0, std::ptr::null_mut(), 0);
    }
}

#[test]
fn lprof_write_data_export() {
    let header_size = size_of::<xcov::types::LlvmProfileHeader>();
    let mut buf = vec![0u8; header_size * 2];
    unsafe {
        let mut writer = xcov::types::ProfDataWriter {
            write_fn: xcov::writer::buffer_writer,
            writer_ctx: buf.as_mut_ptr() as *mut _,
        };
        let result: i32 = xcov::lprofWriteData(&mut writer, std::ptr::null_mut(), 1);
        assert_eq!(result, 0);
    }
}

// =========================================================================
// 5. BEHAVIORAL EQUIVALENCE TESTS
// =========================================================================

#[test]
fn empty_sections_mean_coverage_disabled() {
    assert!(!xcov::coverage_enabled());
}

#[test]
fn merge_rejects_incompatible_data() {
    let buf = [0u8; 256];
    let result = xcov::merge_coverage(&buf);
    assert!(result.is_err());
}

#[test]
fn check_compatibility_rejects_small_buffer() {
    let buf = [0u8; 4];
    let result =
        unsafe { xcov::__llvm_profile_check_compatibility(buf.as_ptr(), buf.len() as u64) };
    assert_eq!(result, -1);
}

#[test]
fn check_compatibility_rejects_bad_magic() {
    let buf = [0u8; 256];
    let result =
        unsafe { xcov::__llvm_profile_check_compatibility(buf.as_ptr(), buf.len() as u64) };
    assert_eq!(result, -1);
}

#[test]
fn get_magic_returns_nonzero() {
    let magic = xcov::__llvm_profile_get_magic();
    assert_ne!(magic, 0);
    assert_eq!(magic >> 56, 0xFF);
}

#[test]
fn get_version_includes_variant_masks() {
    let version = xcov::__llvm_profile_get_version();
    assert_eq!(version & xcov::VARIANT_MASKS_ALL, xcov::VARIANT_MASKS_ALL);
}

#[test]
fn padding_bytes_are_correct() {
    assert_eq!(xcov::__llvm_profile_get_num_padding_bytes(0), 0);
    assert_eq!(xcov::__llvm_profile_get_num_padding_bytes(8), 0);
    assert_eq!(xcov::__llvm_profile_get_num_padding_bytes(1), 7);
    assert_eq!(xcov::__llvm_profile_get_num_padding_bytes(9), 7);
}

#[test]
fn buffer_size_calculation() {
    let size = xcov::__llvm_profile_get_size_for_buffer();
    assert_eq!(size, size_of::<xcov::types::LlvmProfileHeader>() as u64);
}

#[test]
fn data_and_counters_sizes() {
    let data_size = xcov::__llvm_profile_get_data_size();
    let counters_size = xcov::__llvm_profile_get_counters_size();
    let num_data = xcov::__llvm_profile_get_num_data();
    assert_eq!(data_size, 0);
    assert_eq!(counters_size, 0);
    assert_eq!(num_data, 0);
}

#[test]
fn counter_entry_size_normal_mode() {
    assert_eq!(
        xcov::port::counter_entry_size(xcov::INSTR_PROF_RAW_VERSION),
        8
    );
}

#[test]
fn counter_entry_size_byte_coverage() {
    let version = xcov::INSTR_PROF_RAW_VERSION | xcov::types::VARIANT_MASK_BYTE_COVERAGE;
    assert_eq!(xcov::port::counter_entry_size(version), 1);
}

#[test]
fn module_signature_is_deterministic() {
    let sig1 = xcov::module_signature();
    let sig2 = xcov::module_signature();
    assert_eq!(sig1, sig2);
}

// =========================================================================
// 6. PORTABILITY LAYER TESTS
// =========================================================================

#[test]
fn atomic_cas_u64() {
    let mut val: u64 = 42;
    assert!(xcov::port::bool_cmpxchg_u64(&mut val, 42, 100));
    assert_eq!(val, 100);
    assert!(!xcov::port::bool_cmpxchg_u64(&mut val, 42, 200));
    assert_eq!(val, 100);
}

#[test]
fn mem_operations() {
    let mut buf = [0xFFu8; 16];
    unsafe { xcov::port::mem_zero(buf.as_mut_ptr(), buf.len()) };
    assert!(buf.iter().all(|&b| b == 0));

    unsafe { xcov::port::mem_set(buf.as_mut_ptr(), 0xAB, buf.len()) };
    assert!(buf.iter().all(|&b| b == 0xAB));

    let src = [1u8, 2, 3, 4];
    let mut dst = [0u8; 4];
    unsafe { xcov::port::mem_copy(dst.as_mut_ptr(), src.as_ptr(), 4) };
    assert_eq!(dst, src);

    assert_eq!(
        unsafe { xcov::port::mem_cmp(src.as_ptr(), dst.as_ptr(), 4) },
        0
    );
    let other = [1u8, 2, 0, 4];
    assert!(unsafe { xcov::port::mem_cmp(src.as_ptr(), other.as_ptr(), 4) } > 0);
}

#[test]
fn page_size_stub() {
    assert_eq!(xcov::port::get_page_size(), 1);
    assert_eq!(xcov::port::calculate_bytes_needed_to_page_align(0, 4096), 0);
    assert_eq!(
        xcov::port::calculate_bytes_needed_to_page_align(100, 4096),
        3996
    );
    assert_eq!(
        xcov::port::calculate_bytes_needed_to_page_align(4096, 4096),
        0
    );
}

// =========================================================================
// 7. VALUE PROFILING LOGIC TESTS
// =========================================================================

#[test]
fn range_rep_value_matches_llvm() {
    use xcov::value::get_range_rep_value;

    // Values <= 8 are returned as-is.
    for v in 0..=8u64 {
        assert_eq!(get_range_rep_value(v), v, "value {v}");
    }

    // Power of 2: returned as-is (matching C InstrProfGetRangeRepValue).
    assert_eq!(get_range_rep_value(16), 16);
    assert_eq!(get_range_rep_value(32), 32);
    assert_eq!(get_range_rep_value(64), 64);
    assert_eq!(get_range_rep_value(128), 128);
    assert_eq!(get_range_rep_value(256), 256);
    assert_eq!(get_range_rep_value(512), 512);
    assert_eq!(get_range_rep_value(1024), 513); // >= 513 → 513

    // Non-power-of-2: prev_pow2 + 1.
    assert_eq!(get_range_rep_value(9), 9); // 8 + 1
    assert_eq!(get_range_rep_value(15), 9); // 8 + 1
    assert_eq!(get_range_rep_value(17), 17); // 16 + 1
    assert_eq!(get_range_rep_value(300), 257); // 256 + 1
    assert_eq!(get_range_rep_value(1000), 513); // >= 513 → 513
}

// =========================================================================
// 8. BINARY SYMBOL VERIFICATION (via nm)
// =========================================================================

#[test]
fn all_no_mangle_symbols_are_exported() {
    let exe = std::env::current_exe().unwrap();
    let output = std::process::Command::new("nm")
        .arg("-gU")
        .arg(&exe)
        .output()
        .expect("failed to run nm");

    let stdout = String::from_utf8_lossy(&output.stdout);

    let required_symbols = [
        "__llvm_profile_begin_bitmap",
        "__llvm_profile_begin_counters",
        "__llvm_profile_begin_data",
        "__llvm_profile_begin_names",
        "__llvm_profile_begin_vnodes",
        "__llvm_profile_begin_vtables",
        "__llvm_profile_begin_vtabnames",
        "__llvm_profile_end_bitmap",
        "__llvm_profile_end_counters",
        "__llvm_profile_end_data",
        "__llvm_profile_end_names",
        "__llvm_profile_end_vnodes",
        "__llvm_profile_end_vtables",
        "__llvm_profile_end_vtabnames",
        "__llvm_profile_get_counters_size",
        "__llvm_profile_get_data_size",
        "__llvm_profile_get_magic",
        "__llvm_profile_get_num_data",
        "__llvm_profile_get_num_padding_bytes",
        "__llvm_profile_get_num_padding_bytes_for_counters",
        "__llvm_profile_get_size_for_buffer",
        "__llvm_profile_get_version",
        "__llvm_profile_initialize",
        "__llvm_profile_instrument_memop",
        "__llvm_profile_instrument_target",
        "__llvm_profile_instrument_target_value",
        "__llvm_profile_is_continuous_mode_enabled",
        "__llvm_profile_merge_from_buffer",
        "__llvm_profile_raw_version",
        "__llvm_profile_reset_counters",
        "__llvm_profile_runtime",
        "__llvm_profile_set_dumped",
        "__llvm_profile_set_page_size",
        "__llvm_profile_write_buffer",
        "lprofGetLoadModuleSignature",
        "lprofGetVPDataReader",
        "lprofWriteData",
        "minicov_alloc_zeroed",
        "minicov_dealloc",
    ];

    let mut missing = Vec::new();
    for sym in &required_symbols {
        if !stdout.contains(sym) {
            missing.push(*sym);
        }
    }

    assert!(
        missing.is_empty(),
        "Missing #[no_mangle] symbols: {missing:?}"
    );
}

// =========================================================================
// 9. WRITE OUTPUT FORMAT VERIFICATION
// =========================================================================

#[test]
fn written_header_has_correct_magic_and_version() {
    let header_size = size_of::<xcov::types::LlvmProfileHeader>();
    let mut buf = vec![0u8; header_size * 2];
    let result = unsafe { xcov::__llvm_profile_write_buffer(buf.as_mut_ptr()) };
    assert_eq!(result, 0);

    // Read back the header.
    let magic = u64::from_le_bytes(buf[0..8].try_into().unwrap());
    let version = u64::from_le_bytes(buf[8..16].try_into().unwrap());

    assert_eq!(magic, xcov::__llvm_profile_get_magic());
    assert_eq!(version & xcov::VARIANT_MASKS_ALL, xcov::VARIANT_MASKS_ALL);
}

#[test]
fn written_header_counts_match_getters() {
    let header_size = size_of::<xcov::types::LlvmProfileHeader>();
    let mut buf = vec![0u8; header_size * 2];
    let result = unsafe { xcov::__llvm_profile_write_buffer(buf.as_mut_ptr()) };
    assert_eq!(result, 0);

    let num_data = u64::from_le_bytes(buf[24..32].try_into().unwrap());
    let num_counters = u64::from_le_bytes(buf[40..48].try_into().unwrap());

    assert_eq!(num_data, xcov::__llvm_profile_get_num_data());
    assert_eq!(num_counters, xcov::__llvm_profile_get_counters_size() / 8);
}

// =========================================================================
// 10. FEATURE FLAG TESTS
// =========================================================================

#[test]
fn alloc_feature_enabled_by_default() {
    // minicov_alloc_zeroed returns non-null when alloc feature is on.
    unsafe {
        let ptr = xcov::minicov_alloc_zeroed(128, 16);
        assert!(!ptr.is_null());
        // Verify zeroed.
        let slice = std::slice::from_raw_parts(ptr, 128);
        assert!(slice.iter().all(|&b| b == 0));
        xcov::minicov_dealloc(ptr, 128, 16);
    }
}

#[test]
fn capture_coverage_uses_alloc() {
    // capture_coverage internally uses Vec (requires alloc feature).
    let mut buf: Vec<u8> = Vec::new();
    let _ = unsafe { xcov::capture_coverage(&mut buf) };
    // Should not panic — proves alloc feature is working.
}

// =========================================================================
// 11. SECTION NAME CONSISTENCY WITH MINICOV C SOURCE
// =========================================================================
// The ELF section names must match minicov's C code exactly (InstrProfData.inc).
// If these change, the linker script and __start_/__stop_ symbols will break.
//
// minicov C section names (from InstrProfData.inc):
//   __llvm_prf_data   (INSTR_PROF_DATA_COMMON)
//   __llvm_prf_cnts   (INSTR_PROF_CNTS_COMMON)
//   __llvm_prf_bits   (INSTR_PROF_BITS_COMMON)   ← NOT __llvm_prf_bitmap
//   __llvm_prf_names  (INSTR_PROF_NAME_COMMON)
//   __llvm_prf_vnds   (INSTR_PROF_VNODES_COMMON)  ← NOT __llvm_prf_vnodes
//   __llvm_prf_vns    (INSTR_PROF_VNAME_COMMON)
//   __llvm_prf_vals   (INSTR_PROF_VALS_COMMON)
//   __llvm_prf_vtab   (INSTR_PROF_VTAB_COMMON)
// =========================================================================

/// Verifies the Linux/ELF section names in xcov match minicov's C source.
/// This test validates the section names by checking that the object file
/// contains the correct `__start_` / `__stop_` references.
/// Only runs on ELF platforms (Linux) — macOS uses static buffers.
#[cfg(target_os = "linux")]
#[test]
fn section_names_match_minicov_c_source() {
    let exe = std::env::current_exe().unwrap();
    let output = std::process::Command::new("nm")
        .arg("-gU")
        .arg(&exe)
        .output()
        .expect("failed to run nm");
    let stdout = String::from_utf8_lossy(&output.stdout);

    // These MUST match InstrProfData.inc section names, NOT LLVM's "standard" names.
    let required_section_boundaries = [
        "__start___llvm_prf_data",
        "__stop___llvm_prf_data",
        "__start___llvm_prf_cnts",
        "__stop___llvm_prf_cnts",
        "__start___llvm_prf_bits", // NOT __llvm_prf_bitmap
        "__stop___llvm_prf_bits",
        "__start___llvm_prf_names",
        "__stop___llvm_prf_names",
        "__start___llvm_prf_vnds", // NOT __llvm_prf_vnodes
        "__stop___llvm_prf_vnds",
    ];

    let mut missing = Vec::new();
    for sym in &required_section_boundaries {
        if !stdout.contains(sym) {
            missing.push(*sym);
        }
    }
    assert!(
        missing.is_empty(),
        "Missing section boundary symbols: {missing:?}"
    );

    // Verify the WRONG names are NOT present.
    let wrong_names = [
        "__start___llvm_prf_bitmap",
        "__stop___llvm_prf_bitmap",
        "__start___llvm_prf_vnodes",
        "__stop___llvm_prf_vnodes",
    ];
    let mut found_wrong = Vec::new();
    for sym in &wrong_names {
        if stdout.contains(sym) {
            found_wrong.push(*sym);
        }
    }
    assert!(
        found_wrong.is_empty(),
        "Found WRONG section names (should use minicov's names, not LLVM standard): \
         {found_wrong:?}"
    );
}
