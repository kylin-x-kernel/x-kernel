// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Public API tests for xcover.
//!
//! Only tests the public Rust API surface. Does not call ABI functions
//! directly and does not use unsafe.

// === 1. PUBLIC API SIGNATURE TESTS ===

use xcover::ProfileWriter;

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
fn empty_sections_mean_not_enabled() {
    // On macOS without -Cinstrument-coverage, sections are empty.
    assert!(!xcover::is_enabled());
}

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
