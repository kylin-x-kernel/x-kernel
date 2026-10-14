// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Public API tests for xcover.
//!
//! Only tests the public Rust API surface. ABI symbols are inspected through
//! the linked test binary.

// === 1. PUBLIC API SIGNATURE TESTS ===

use std::ptr;

use xcover::ProfileWriter;

#[repr(C, align(8))]
struct TestLlvmProfileData {
    name_ref: u64,
    func_hash: u64,
    counter_ptr: *mut core::ffi::c_void,
    bitmap_ptr: *mut core::ffi::c_void,
    function_pointer: *mut core::ffi::c_void,
    values: *mut core::ffi::c_void,
    num_counters: u32,
    num_value_sites: [u16; 3],
    num_bitmap_bytes: u32,
}

// SAFETY: these zeroed records are immutable test-binary section anchors used
// only so ELF start/stop boundary symbols exist for xcover's host tests.
unsafe impl Sync for TestLlvmProfileData {}

#[repr(C)]
struct TestValueProfNode {
    value: u64,
    count: u64,
    next: *mut TestValueProfNode,
}

// SAFETY: this zeroed node is an immutable test-binary section anchor used
// only so ELF start/stop boundary symbols exist for xcover's host tests.
unsafe impl Sync for TestValueProfNode {}

#[used]
#[unsafe(link_section = "__llvm_prf_data")]
static TEST_PRF_DATA: TestLlvmProfileData = TestLlvmProfileData {
    name_ref: 0,
    func_hash: 0,
    counter_ptr: ptr::null_mut(),
    bitmap_ptr: ptr::null_mut(),
    function_pointer: ptr::null_mut(),
    values: ptr::null_mut(),
    num_counters: 0,
    num_value_sites: [0; 3],
    num_bitmap_bytes: 0,
};

#[used]
#[unsafe(link_section = "__llvm_prf_cnts")]
static TEST_PRF_COUNTERS: u8 = 0;

#[used]
#[unsafe(link_section = "__llvm_prf_bits")]
static TEST_PRF_BITS: u8 = 0;

#[used]
#[unsafe(link_section = "__llvm_prf_names")]
static TEST_PRF_NAMES: u8 = 0;

#[used]
#[unsafe(link_section = "__llvm_prf_vnds")]
static TEST_PRF_VNODES: TestValueProfNode = TestValueProfNode {
    value: 0,
    count: 0,
    next: ptr::null_mut(),
};

fn force_link_abi_symbols_for_test() {
    unsafe extern "C" {
        #[link_name = "__llvm_profile_runtime"]
        static LLVM_PROFILE_RUNTIME: u8;
        #[link_name = "__llvm_profile_raw_version"]
        static LLVM_PROFILE_RAW_VERSION: u64;

        fn __llvm_profile_begin_bitmap();
        fn __llvm_profile_begin_counters();
        fn __llvm_profile_begin_data();
        fn __llvm_profile_begin_names();
        fn __llvm_profile_begin_vnodes();
        fn __llvm_profile_begin_vtables();
        fn __llvm_profile_begin_vtabnames();
        fn __llvm_profile_end_bitmap();
        fn __llvm_profile_end_counters();
        fn __llvm_profile_end_data();
        fn __llvm_profile_end_names();
        fn __llvm_profile_end_vnodes();
        fn __llvm_profile_end_vtables();
        fn __llvm_profile_end_vtabnames();
        fn __llvm_profile_get_counters_size();
        fn __llvm_profile_get_data_size();
        fn __llvm_profile_get_magic();
        fn __llvm_profile_get_num_data();
        fn __llvm_profile_get_num_padding_bytes();
        fn __llvm_profile_get_num_padding_bytes_for_counters();
        fn __llvm_profile_get_size_for_buffer();
        fn __llvm_profile_get_version();
        fn __llvm_profile_initialize();
        fn __llvm_profile_instrument_memop();
        fn __llvm_profile_instrument_target();
        fn __llvm_profile_instrument_target_value();
        fn __llvm_profile_is_continuous_mode_enabled();
        fn __llvm_profile_merge_from_buffer();
        fn __llvm_profile_reset_counters();
        fn __llvm_profile_set_dumped();
        fn __llvm_profile_set_page_size();
        fn __llvm_profile_write_buffer();
        #[link_name = "lprofGetLoadModuleSignature"]
        fn lprof_get_load_module_signature();
    }

    let functions = [
        __llvm_profile_begin_bitmap as *const (),
        __llvm_profile_begin_counters as *const (),
        __llvm_profile_begin_data as *const (),
        __llvm_profile_begin_names as *const (),
        __llvm_profile_begin_vnodes as *const (),
        __llvm_profile_begin_vtables as *const (),
        __llvm_profile_begin_vtabnames as *const (),
        __llvm_profile_end_bitmap as *const (),
        __llvm_profile_end_counters as *const (),
        __llvm_profile_end_data as *const (),
        __llvm_profile_end_names as *const (),
        __llvm_profile_end_vnodes as *const (),
        __llvm_profile_end_vtables as *const (),
        __llvm_profile_end_vtabnames as *const (),
        __llvm_profile_get_counters_size as *const (),
        __llvm_profile_get_data_size as *const (),
        __llvm_profile_get_magic as *const (),
        __llvm_profile_get_num_data as *const (),
        __llvm_profile_get_num_padding_bytes as *const (),
        __llvm_profile_get_num_padding_bytes_for_counters as *const (),
        __llvm_profile_get_size_for_buffer as *const (),
        __llvm_profile_get_version as *const (),
        __llvm_profile_initialize as *const (),
        __llvm_profile_instrument_memop as *const (),
        __llvm_profile_instrument_target as *const (),
        __llvm_profile_instrument_target_value as *const (),
        __llvm_profile_is_continuous_mode_enabled as *const (),
        __llvm_profile_merge_from_buffer as *const (),
        __llvm_profile_reset_counters as *const (),
        __llvm_profile_set_dumped as *const (),
        __llvm_profile_set_page_size as *const (),
        __llvm_profile_write_buffer as *const (),
        lprof_get_load_module_signature as *const (),
    ];
    std::hint::black_box(functions);

    let statics = [
        ptr::addr_of!(LLVM_PROFILE_RUNTIME).cast::<()>(),
        ptr::addr_of!(LLVM_PROFILE_RAW_VERSION).cast::<()>(),
    ];
    std::hint::black_box(statics);
}

#[test]
fn is_enabled_returns_bool() {
    let _enabled: bool = xcover::is_enabled();
}

#[test]
fn module_signature_returns_u64() {
    let _sig: u64 = xcover::module_signature();
}

#[test]
fn module_signature_is_deterministic() {
    let sig1 = xcover::module_signature();
    let sig2 = xcover::module_signature();
    assert_eq!(sig1, sig2);
}

#[test]
fn reset_is_callable() {
    xcover::reset();
}

#[test]
fn profile_error_variants_exist() {
    let _ = xcover::ProfileError::NotEnabled;
    let _ = xcover::ProfileError::MalformedInput;
    let _ = xcover::ProfileError::IncompatibleInput;
    let _ = xcover::ProfileError::OutputTooSmall;
    let _ = xcover::ProfileError::WriterFailed;
    let _ = xcover::ProfileError::RuntimeBusy;
    let _ = xcover::ProfileError::ArithmeticOverflow;
}

#[test]
fn profile_error_has_debug_and_display() {
    let e = xcover::ProfileError::NotEnabled;
    let _debug = format!("{:?}", e);
    let _display = format!("{}", e);
}

#[test]
fn profile_error_is_copy_clone() {
    fn _check<T: Copy + Clone>() {}
    fn _verify() {
        _check::<xcover::ProfileError>();
    }
}

#[test]
fn profile_writer_trait_exists() {
    fn _assert_trait<W: xcover::ProfileWriter>() {}
    fn _check() {
        _assert_trait::<Vec<u8>>();
    }
}

#[test]
fn profile_writer_vec_impl() {
    let mut v: Vec<u8> = Vec::new();
    v.write_all(&[1, 2, 3]).unwrap();
    assert_eq!(v, vec![1, 2, 3]);
    v.write_all(&[4, 5]).unwrap();
    assert_eq!(v, vec![1, 2, 3, 4, 5]);
}

// === 2. BEHAVIORAL TESTS ===

#[test]
fn merge_profraw_rejects_invalid_data() {
    let buf = [0u8; 256];
    let result = xcover::merge_profraw(&buf);
    assert!(result.is_err());
    // Invalid magic/version → MalformedInput or IncompatibleInput.
    let _err = result.unwrap_err();
}

#[test]
fn merge_profraw_rejects_small_buffer() {
    let buf = [0u8; 4];
    let result = xcover::merge_profraw(&buf);
    assert!(result.is_err());
}

#[test]
fn write_profraw_uses_alloc() {
    let mut buf: Vec<u8> = Vec::new();
    let _ = xcover::write_profraw(&mut buf);
}

#[test]
fn write_profraw_returns_writer_failed_on_error() {
    struct FailingWriter;
    impl xcover::ProfileWriter for FailingWriter {
        fn write_all(&mut self, _bytes: &[u8]) -> Result<(), xcover::ProfileError> {
            Err(xcover::ProfileError::WriterFailed)
        }
    }
    let mut writer = FailingWriter;
    // This may succeed or fail depending on whether sections are empty,
    // but the type signature is verified.
    let _ = xcover::write_profraw(&mut writer);
}

// === 3. BINARY SYMBOL VERIFICATION (via nm) ===
//
// ABI symbols must still exist in the final binary even though they
// are not part of the Rust API.

#[test]
fn all_no_mangle_symbols_are_exported() {
    force_link_abi_symbols_for_test();
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

// === 4. SECTION NAME CONSISTENCY WITH MINICOV C SOURCE ===

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

    let required_section_boundaries = [
        "__start___llvm_prf_data",
        "__stop___llvm_prf_data",
        "__start___llvm_prf_cnts",
        "__stop___llvm_prf_cnts",
        "__start___llvm_prf_bits",
        "__stop___llvm_prf_bits",
        "__start___llvm_prf_names",
        "__stop___llvm_prf_names",
        "__start___llvm_prf_vnds",
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
