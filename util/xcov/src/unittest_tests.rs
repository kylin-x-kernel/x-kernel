// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel-mode unittests for xcov.
//!
//! Converted from tests/ffi_equivalence.rs to run via the kernel's
//! unittest framework during `make run UNITTEST=y`.

#[cfg(unittest)]
mod layout_tests {
    use core::mem::{align_of, offset_of, size_of};

    use unittest::{assert_eq, def_test};

    use crate::types::*;

    #[def_test]
    fn struct_sizes_match() {
        #[cfg(target_pointer_width = "64")]
        assert_eq!(size_of::<LlvmProfileData>(), 64);
        assert_eq!(align_of::<LlvmProfileData>(), 8);
        assert_eq!(size_of::<LlvmProfileHeader>(), 128);
        assert_eq!(size_of::<ValueProfNode>(), 24);
        assert_eq!(size_of::<InstrProfValueData>(), 16);
    }

    #[def_test]
    fn llvm_profile_data_field_offsets() {
        assert_eq!(offset_of!(LlvmProfileData, name_ref), 0);
        assert_eq!(offset_of!(LlvmProfileData, func_hash), 8);
        assert_eq!(offset_of!(LlvmProfileData, counter_ptr), 16);
        assert_eq!(offset_of!(LlvmProfileData, bitmap_ptr), 24);
        assert_eq!(offset_of!(LlvmProfileData, function_pointer), 32);
        assert_eq!(offset_of!(LlvmProfileData, values), 40);
        assert_eq!(offset_of!(LlvmProfileData, num_counters), 48);
        assert_eq!(offset_of!(LlvmProfileData, num_value_sites), 52);
        assert_eq!(offset_of!(LlvmProfileData, num_bitmap_bytes), 60);
    }

    #[def_test]
    fn llvm_profile_header_field_offsets() {
        assert_eq!(offset_of!(LlvmProfileHeader, magic), 0);
        assert_eq!(offset_of!(LlvmProfileHeader, version), 8);
        assert_eq!(offset_of!(LlvmProfileHeader, binary_ids_size), 16);
        assert_eq!(offset_of!(LlvmProfileHeader, num_data), 24);
        assert_eq!(
            offset_of!(LlvmProfileHeader, padding_bytes_before_counters),
            32
        );
        assert_eq!(offset_of!(LlvmProfileHeader, num_counters), 40);
        assert_eq!(
            offset_of!(LlvmProfileHeader, padding_bytes_after_counters),
            48
        );
        assert_eq!(offset_of!(LlvmProfileHeader, num_bitmap_bytes), 56);
        assert_eq!(
            offset_of!(LlvmProfileHeader, padding_bytes_after_bitmap_bytes),
            64
        );
        assert_eq!(offset_of!(LlvmProfileHeader, names_size), 72);
        assert_eq!(offset_of!(LlvmProfileHeader, counters_delta), 80);
        assert_eq!(offset_of!(LlvmProfileHeader, bitmap_delta), 88);
        assert_eq!(offset_of!(LlvmProfileHeader, names_delta), 96);
        assert_eq!(offset_of!(LlvmProfileHeader, num_vtables), 104);
        assert_eq!(offset_of!(LlvmProfileHeader, vnames_size), 112);
        assert_eq!(offset_of!(LlvmProfileHeader, value_kind_last), 120);
    }

    #[def_test]
    fn value_prof_node_field_offsets() {
        assert_eq!(offset_of!(ValueProfNode, value), 0);
        assert_eq!(offset_of!(ValueProfNode, count), 8);
        assert_eq!(offset_of!(ValueProfNode, next), 16);
    }

    #[def_test]
    fn instr_prof_value_data_field_offsets() {
        assert_eq!(offset_of!(InstrProfValueData, value), 0);
        assert_eq!(offset_of!(InstrProfValueData, count), 8);
    }

    #[def_test]
    fn writer_struct_sizes() {
        assert_eq!(size_of::<ProfDataWriter>(), 16);
        assert_eq!(size_of::<ProfDataIOVec>(), 32);
        assert_eq!(size_of::<ValueProfData>(), 8);
        assert_eq!(size_of::<ValueProfRecord>(), 8);
    }
}

#[cfg(unittest)]
mod constant_tests {
    use unittest::def_test;

    use crate::types::*;

    #[def_test]
    fn constants_match_llvm() {
        assert_eq!(INSTR_PROF_RAW_VERSION, 10);
        assert_eq!(VARIANT_MASKS_ALL, 0xffff_ffff_0000_0000);
        assert_eq!(VARIANT_MASK_IR_PROF, 1u64 << 56);
        assert_eq!(VARIANT_MASK_BYTE_COVERAGE, 1u64 << 60);
        assert_eq!(VARIANT_MASK_TEMPORAL_PROF, 1u64 << 63);
        assert_eq!(IPVK_INDIRECT_CALL_TARGET, 0);
        assert_eq!(IPVK_MEM_OP_SIZE, 1);
        assert_eq!(IPVK_VTABLE_TARGET, 2);
        assert_eq!(IPVK_FIRST, 0);
        assert_eq!(IPVK_LAST, 2);
        assert_eq!(IPVK_NUM_KINDS, 3);
        assert_eq!(INSTR_PROF_DEFAULT_NUM_VAL_PER_SITE, 24);
        assert_eq!(INSTR_PROF_DATA_ALIGNMENT, 8);
    }

    #[def_test]
    fn padding_matches_c_runtime() {
        assert_eq!(get_num_padding_bytes(0), 0);
        assert_eq!(get_num_padding_bytes(1), 7);
        assert_eq!(get_num_padding_bytes(7), 1);
        assert_eq!(get_num_padding_bytes(8), 0);
        assert_eq!(get_num_padding_bytes(9), 7);
        assert_eq!(get_num_padding_bytes(16), 0);
        assert_eq!(get_num_padding_bytes(17), 7);
        assert_eq!(get_num_padding_bytes(24), 0);
        assert_eq!(get_num_padding_bytes(100), 4);
    }

    #[def_test]
    fn magic_number_properties() {
        assert_eq!(INSTR_PROF_RAW_MAGIC_64 >> 56, 0xFF);
        assert_eq!(INSTR_PROF_RAW_MAGIC_32 >> 56, 0xFF);
        assert_ne!(INSTR_PROF_RAW_MAGIC_64, INSTR_PROF_RAW_MAGIC_32);
    }

    #[def_test]
    fn re_exported_constants_at_crate_root() {
        assert_eq!(crate::INSTR_PROF_RAW_VERSION, 10);
        assert_eq!(crate::VARIANT_MASKS_ALL, 0xffff_ffff_0000_0000);
    }
}

#[cfg(unittest)]
mod export_tests {
    use core::{mem::size_of, ptr};

    use unittest::def_test;

    use crate::types::*;

    #[def_test]
    fn static_exports_exist() {
        let _val: u8 = crate::__llvm_profile_runtime;
        let _ver: u64 = crate::__llvm_profile_raw_version;
    }

    #[def_test]
    fn section_accessor_exports() {
        // SAFETY: each call is a read-only FFI accessor that returns a pointer into
        // the linker-defined coverage sections; none of them dereference the result
        // or mutate global state, so invoking them with no arguments is sound.
        unsafe {
            let _: *const u8 = crate::__llvm_profile_begin_counters();
            let _: *const u8 = crate::__llvm_profile_end_counters();
            let _: *const LlvmProfileData = crate::__llvm_profile_begin_data();
            let _: *const LlvmProfileData = crate::__llvm_profile_end_data();
            let _: *const u8 = crate::__llvm_profile_begin_names();
            let _: *const u8 = crate::__llvm_profile_end_names();
            let _: *const u8 = crate::__llvm_profile_begin_bitmap();
            let _: *const u8 = crate::__llvm_profile_end_bitmap();
            let _: *const ValueProfNode = crate::__llvm_profile_begin_vnodes();
            let _: *const ValueProfNode = crate::__llvm_profile_end_vnodes();
            let _: *const u8 = crate::__llvm_profile_begin_vtables();
            let _: *const u8 = crate::__llvm_profile_end_vtables();
            let _: *const u8 = crate::__llvm_profile_begin_vtabnames();
            let _: *const u8 = crate::__llvm_profile_end_vtabnames();
        }
    }

    #[def_test]
    fn profiling_function_exports() {
        // SAFETY: both FFI calls receive a null pointer with length 0, which the
        // compatibility/merge routines treat as an empty buffer (returning an
        // error code without dereferencing the pointer); no global state is
        // mutated in a way observable by this test.
        unsafe {
            let _: i32 = crate::__llvm_profile_merge_from_buffer(ptr::null(), 0);
            let _: i32 = crate::__llvm_profile_check_compatibility(ptr::null(), 0);
        }
        let _: u64 = crate::__llvm_profile_get_version();
        let _: u64 = crate::__llvm_profile_get_magic();
        let _: u8 = crate::__llvm_profile_get_num_padding_bytes(8);
        let _: u64 = crate::__llvm_profile_get_size_for_buffer();
        let _: u64 = crate::__llvm_profile_get_data_size();
        let _: u64 = crate::__llvm_profile_get_counters_size();
        let _: u64 = crate::__llvm_profile_get_num_data();
        let _: u64 = crate::__llvm_profile_get_num_padding_bytes_for_counters();
    }

    #[def_test]
    fn continuous_mode_exports() {
        assert_eq!(crate::__llvm_profile_is_continuous_mode_enabled(), 0);
        crate::__llvm_profile_enable_continuous_mode();
        crate::__llvm_profile_disable_continuous_mode();
        crate::__llvm_profile_set_page_size(4096);
        assert_eq!(crate::__llvm_profile_is_continuous_mode_enabled(), 0);
    }

    #[def_test]
    fn dumped_flag_exports() {
        crate::__llvm_profile_set_dumped();
        crate::__llvm_profile_initialize();
    }

    #[def_test]
    fn allocator_hook_exports() {
        // SAFETY: alloc_zeroed(64, 8) requests 64 bytes with 8-byte alignment from
        // the xcov allocator; the assertion confirms a non-null return before the
        // matching dealloc reuses the same (size, align) pair, satisfying the
        // allocator contract of freeing exactly what was allocated.
        unsafe {
            let ptr = crate::minicov_alloc_zeroed(64, 8);
            assert!(!ptr.is_null());
            crate::minicov_dealloc(ptr, 64, 8);
        }
    }

    #[def_test]
    fn lprof_exports() {
        let _: *mut VPDataReaderType = crate::lprofGetVPDataReader();
        let _: u64 = crate::lprofGetLoadModuleSignature();
    }

    #[def_test]
    fn value_profiling_exports() {
        // SAFETY: all three instrumentation FFI calls accept a null data pointer;
        // per the LLVM profiling contract, when the data pointer is null the
        // routine is a no-op (it records no value profiling site), so no pointer
        // is dereferenced and no global state is mutated.
        unsafe {
            crate::__llvm_profile_instrument_target(0, ptr::null_mut(), 0);
            crate::__llvm_profile_instrument_target_value(0, ptr::null_mut(), 0, 0);
            crate::__llvm_profile_instrument_memop(0, ptr::null_mut(), 0);
        }
    }

    #[def_test]
    fn lprof_write_data_export() {
        let buf_size = crate::__llvm_profile_get_size_for_buffer() as usize;
        let alloc_size = if buf_size > 0 {
            buf_size
        } else {
            size_of::<LlvmProfileHeader>() * 2
        };
        let mut buf = alloc::vec![0u8; alloc_size];
        // SAFETY: buf is a freshly zero-initialized Vec of alloc_size bytes that
        // remains live for the duration of the block; writer_ctx borrows buf's
        // backing storage exclusively (no aliasing references exist) and
        // buffer_writer writes within [0, alloc_size); the third argument to
        // lprofWriteData is the contiguous-mode flag, so the FFI fills buf via the
        // writer and never outlives the borrow.
        unsafe {
            let mut writer = ProfDataWriter {
                write_fn: crate::writer::buffer_writer,
                writer_ctx: buf.as_mut_ptr() as *mut _,
            };
            let result: i32 = crate::lprofWriteData(&mut writer, ptr::null_mut(), 1);
            assert_eq!(result, 0);
        }
    }
}

#[cfg(unittest)]
mod behavior_tests {
    use core::mem::size_of;

    use unittest::def_test;

    use crate::types::*;

    #[def_test]
    fn coverage_enabled_returns_bool() {
        let _enabled: bool = crate::coverage_enabled();
    }

    #[def_test]
    fn module_signature_returns_u64() {
        let _sig: u64 = crate::module_signature();
    }

    #[def_test]
    fn merge_rejects_incompatible_data() {
        let buf = [0u8; 256];
        let result = crate::merge_coverage(&buf);
        assert!(result.is_err());
    }

    #[def_test]
    fn check_compatibility_rejects_small_buffer() {
        let buf = [0u8; 4];
        // SAFETY: buf is a 4-byte stack array whose length (4) is below the header
        // size the compatibility routine requires, so the FFI never reads beyond
        // buf and simply returns -1; buf.as_ptr() is valid and aligned for u8 for
        // the duration of the call.
        let result =
            unsafe { crate::__llvm_profile_check_compatibility(buf.as_ptr(), buf.len() as u64) };
        assert_eq!(result, -1);
    }

    #[def_test]
    fn check_compatibility_rejects_bad_magic() {
        let buf = [0u8; 256];
        // SAFETY: buf is a 256-byte stack array, large enough to read the header
        // fields; it is zero-initialized so the magic field check fails early and
        // the routine returns -1 without dereferencing any embedded pointers.
        // buf.as_ptr() stays valid and properly aligned for u8 for the call.
        let result =
            unsafe { crate::__llvm_profile_check_compatibility(buf.as_ptr(), buf.len() as u64) };
        assert_eq!(result, -1);
    }

    #[def_test]
    fn get_magic_returns_nonzero() {
        let magic = crate::__llvm_profile_get_magic();
        assert_ne!(magic, 0);
        assert_eq!(magic >> 56, 0xFF);
    }

    #[def_test]
    fn get_version_base_matches_raw_version() {
        let version = crate::__llvm_profile_get_version();
        assert_eq!(
            version & !crate::VARIANT_MASKS_ALL,
            crate::INSTR_PROF_RAW_VERSION
        );
    }

    #[def_test]
    fn padding_bytes_are_correct() {
        assert_eq!(crate::__llvm_profile_get_num_padding_bytes(0), 0);
        assert_eq!(crate::__llvm_profile_get_num_padding_bytes(8), 0);
        assert_eq!(crate::__llvm_profile_get_num_padding_bytes(1), 7);
        assert_eq!(crate::__llvm_profile_get_num_padding_bytes(9), 7);
    }

    #[def_test]
    fn buffer_size_calculation() {
        let size = crate::__llvm_profile_get_size_for_buffer();
        // Must be at least the header size.
        assert!(size >= size_of::<LlvmProfileHeader>() as u64);
    }

    #[def_test]
    fn data_and_counters_sizes() {
        let data_size = crate::__llvm_profile_get_data_size();
        let counters_size = crate::__llvm_profile_get_counters_size();
        let num_data = crate::__llvm_profile_get_num_data();
        // In kernel mode with coverage, sections may be populated.
        // data_size must be a multiple of LlvmProfileData size.
        assert_eq!(data_size % size_of::<LlvmProfileData>() as u64, 0);
        // counters_size must be a multiple of 8 (u64 entries).
        assert_eq!(counters_size % 8, 0);
        // num_data must match data_size / sizeof(LlvmProfileData).
        assert_eq!(num_data, data_size / size_of::<LlvmProfileData>() as u64);
    }

    #[def_test]
    fn counter_entry_size_normal_mode() {
        assert_eq!(
            crate::port::counter_entry_size(crate::INSTR_PROF_RAW_VERSION),
            8
        );
    }

    #[def_test]
    fn counter_entry_size_byte_coverage() {
        let version = crate::INSTR_PROF_RAW_VERSION | crate::types::VARIANT_MASK_BYTE_COVERAGE;
        assert_eq!(crate::port::counter_entry_size(version), 1);
    }

    #[def_test]
    fn module_signature_is_deterministic() {
        let sig1 = crate::module_signature();
        let sig2 = crate::module_signature();
        assert_eq!(sig1, sig2);
    }

    #[def_test]
    fn merge_coverage_signature() {
        let data = [0u8; 64];
        let result = crate::merge_coverage(&data);
        let _typed: Result<(), crate::IncompatibleCoverageData> = result;
    }
}

#[cfg(unittest)]
mod port_tests {
    use unittest::def_test;

    #[def_test]
    fn atomic_cas_u64() {
        let mut val: u64 = 42;
        assert!(crate::port::bool_cmpxchg_u64(&mut val, 42, 100));
        assert_eq!(val, 100);
        assert!(!crate::port::bool_cmpxchg_u64(&mut val, 42, 200));
        assert_eq!(val, 100);
    }

    #[def_test]
    fn mem_operations() {
        let mut buf = [0xFFu8; 16];
        // SAFETY: buf.as_mut_ptr() is a valid, 1-byte-aligned, fully initialized
        // pointer to a 16-byte stack array; mem_zero writes exactly buf.len()
        // bytes (16) inside the array, which is the only outstanding borrow.
        unsafe {
            crate::port::mem_zero(buf.as_mut_ptr(), buf.len());
        }
        assert!(buf.iter().all(|&b| b == 0));

        // SAFETY: same 16-byte buf, again valid/aligned/initialized; mem_set
        // writes exactly buf.len() bytes within bounds and is the sole access.
        unsafe {
            crate::port::mem_set(buf.as_mut_ptr(), 0xAB, buf.len());
        }
        assert!(buf.iter().all(|&b| b == 0xAB));

        let src = [1u8, 2, 3, 4];
        let mut dst = [0u8; 4];
        // SAFETY: dst and src are disjoint 4-byte arrays; both pointers are valid,
        // 1-byte aligned, and initialized for 4 bytes; mem_copy reads 4 bytes from
        // src and writes 4 bytes to dst with no aliasing.
        unsafe {
            crate::port::mem_copy(dst.as_mut_ptr(), src.as_ptr(), 4);
        }
        assert_eq!(dst, src);

        assert_eq!(
            // SAFETY: src and dst are valid 4-byte arrays still alive; mem_cmp
            // only reads within [0, 4) of each and they do not alias in a way
            // that affects a byte-wise comparison.
            unsafe { crate::port::mem_cmp(src.as_ptr(), dst.as_ptr(), 4) },
            0
        );
        let other = [1u8, 2, 0, 4];
        assert!(
            // SAFETY: src and other are valid, 1-byte-aligned, fully initialized
            // 4-byte arrays; mem_cmp reads at most 4 bytes from each, all within
            // bounds, and both outlive the call.
            unsafe { crate::port::mem_cmp(src.as_ptr(), other.as_ptr(), 4) } > 0
        );
    }

    #[def_test]
    fn page_size_stub() {
        assert_eq!(crate::port::get_page_size(), 1);
        assert_eq!(
            crate::port::calculate_bytes_needed_to_page_align(0, 4096),
            0
        );
        assert_eq!(
            crate::port::calculate_bytes_needed_to_page_align(100, 4096),
            3996
        );
        assert_eq!(
            crate::port::calculate_bytes_needed_to_page_align(4096, 4096),
            0
        );
    }
}

#[cfg(unittest)]
mod value_tests {
    use unittest::def_test;

    #[def_test]
    fn range_rep_value_matches_llvm() {
        use crate::value::get_range_rep_value;

        for v in 0..=8u64 {
            assert_eq!(get_range_rep_value(v), v);
        }

        assert_eq!(get_range_rep_value(16), 16);
        assert_eq!(get_range_rep_value(32), 32);
        assert_eq!(get_range_rep_value(64), 64);
        assert_eq!(get_range_rep_value(128), 128);
        assert_eq!(get_range_rep_value(256), 256);
        assert_eq!(get_range_rep_value(512), 512);
        assert_eq!(get_range_rep_value(1024), 513);

        assert_eq!(get_range_rep_value(9), 9);
        assert_eq!(get_range_rep_value(15), 9);
        assert_eq!(get_range_rep_value(17), 17);
        assert_eq!(get_range_rep_value(300), 257);
        assert_eq!(get_range_rep_value(1000), 513);
    }
}

#[cfg(unittest)]
mod write_tests {
    use core::mem::size_of;

    use unittest::def_test;

    use crate::types::*;

    fn alloc_write_buffer() -> alloc::vec::Vec<u8> {
        let buf_size = crate::__llvm_profile_get_size_for_buffer() as usize;
        let alloc_size = if buf_size > size_of::<LlvmProfileHeader>() {
            buf_size
        } else {
            size_of::<LlvmProfileHeader>() * 2
        };
        alloc::vec![0u8; alloc_size]
    }

    #[def_test]
    fn written_header_has_correct_magic_and_version() {
        let mut buf = alloc_write_buffer();
        // SAFETY: buf is a Vec of at least one LlvmProfileHeader worth of bytes
        // (alloc_write_buffer guarantees alloc_size >= header size when the
        // reported buffer size is too small), so __llvm_profile_write_buffer can
        // serialise the header without overflowing; buf.as_mut_ptr() is the sole
        // borrow and remains valid for the call.
        let result = unsafe { crate::__llvm_profile_write_buffer(buf.as_mut_ptr()) };
        assert_eq!(result, 0);

        let magic = u64::from_le_bytes(buf[0..8].try_into().unwrap());
        let version = u64::from_le_bytes(buf[8..16].try_into().unwrap());

        assert_eq!(magic, crate::__llvm_profile_get_magic());
        assert_eq!(
            version & !crate::VARIANT_MASKS_ALL,
            crate::INSTR_PROF_RAW_VERSION
        );
    }

    #[def_test]
    fn written_header_counts_match_getters() {
        let mut buf = alloc_write_buffer();
        // SAFETY: same invariant as written_header_has_correct_magic_and_version:
        // alloc_write_buffer ensures buf holds at least one LlvmProfileHeader, so
        // the write fits; buf.as_mut_ptr() is the exclusive, in-bounds borrow.
        let result = unsafe { crate::__llvm_profile_write_buffer(buf.as_mut_ptr()) };
        assert_eq!(result, 0);

        let num_data = u64::from_le_bytes(buf[24..32].try_into().unwrap());
        let num_counters = u64::from_le_bytes(buf[40..48].try_into().unwrap());

        assert_eq!(num_data, crate::__llvm_profile_get_num_data());
        assert_eq!(
            num_counters,
            crate::buffer::get_num_counters(
                crate::platform::begin_counters(),
                crate::platform::end_counters(),
            )
        );
    }
}

#[cfg(unittest)]
mod api_tests {
    use unittest::def_test;

    use crate::CoverageWriter;

    #[def_test]
    fn public_trait_coverage_writer_exists() {
        fn _assert_trait<W: crate::CoverageWriter>() {}
        fn _check() {
            _assert_trait::<alloc::vec::Vec<u8>>();
        }
    }

    #[def_test]
    fn coverage_writer_vec_impl() {
        let mut v: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        v.write(&[1, 2, 3]).unwrap();
        assert_eq!(v, alloc::vec![1, 2, 3]);
        v.write(&[4, 5]).unwrap();
        assert_eq!(v, alloc::vec![1, 2, 3, 4, 5]);
    }

    #[def_test]
    fn error_types_exist_with_traits() {
        fn _check_copy_clone<T: Copy + Clone>() {}
        fn _check() {
            _check_copy_clone::<crate::CoverageWriteError>();
            _check_copy_clone::<crate::IncompatibleCoverageData>();
        }
    }

    #[def_test]
    fn alloc_feature_enabled_by_default() {
        // SAFETY: minicov_alloc_zeroed(128, 16) returns a freshly allocated,
        // zero-initialised, 16-byte-aligned buffer of 128 bytes; the null check
        // precedes from_raw_parts, so ptr is valid for 128 bytes; the slice only
        // reads within the allocation, and minicov_dealloc is called with the
        // identical (128, 16) pair to free exactly what was allocated.
        unsafe {
            let ptr = crate::minicov_alloc_zeroed(128, 16);
            assert!(!ptr.is_null());
            let slice = core::slice::from_raw_parts(ptr, 128);
            assert!(slice.iter().all(|&b| b == 0));
            crate::minicov_dealloc(ptr, 128, 16);
        }
    }

    #[def_test]
    fn capture_coverage_uses_alloc() {
        let mut buf: alloc::vec::Vec<u8> = alloc::vec::Vec::new();
        let _ = crate::capture_coverage(&mut buf);
    }
}
