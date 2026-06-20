// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Pure Rust code coverage and profile-guided optimization (PGO) support
//! for `no_std` and embedded programs.
//!
//! and all functionality remains identical.

#![no_std]

pub mod buffer;
pub mod internal;
pub mod merge;
pub mod platform;
pub mod port;
pub mod profiling;
pub mod types;
pub mod value;
pub mod version;
pub mod writer;

#[cfg(unittest)]
mod unittest_tests;

use core::ffi::c_void;

use types::*;
// Re-export public constants at crate root.
pub use types::{INSTR_PROF_RAW_VERSION, VARIANT_MASKS_ALL};

// === #[unsafe(no_mangle)] exports required by compiler instrumentation ===

#[unsafe(no_mangle)]
pub static __llvm_profile_runtime: u8 = 0;

#[unsafe(no_mangle)]
pub static __llvm_profile_raw_version: u64 = types::INSTR_PROF_RAW_VERSION;

// === Custom allocator hooks ===

/// Allocates zeroed memory for profiling data.
///
/// # Safety
///
/// `align` must be a valid power-of-two alignment value.
#[cfg(feature = "alloc")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn minicov_alloc_zeroed(size: usize, align: usize) -> *mut u8 {
    // SAFETY: forwards the caller's allocation contract to the portability layer.
    unsafe { port::alloc_zeroed(size, align) }
}

/// Deallocates profiling memory.
///
/// # Safety
///
/// `ptr` must have been returned by a corresponding `minicov_alloc_zeroed` call
/// with the same `size` and `align`.
#[cfg(feature = "alloc")]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn minicov_dealloc(ptr: *mut u8, size: usize, align: usize) {
    // SAFETY: forwards the caller's deallocation contract to the portability layer.
    unsafe { port::dealloc(ptr, size, align) }
}

/// Stub allocator when `alloc` feature is disabled — always returns null.
///
/// # Safety
///
/// Safe to call, but always returns a null pointer.
#[cfg(not(feature = "alloc"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn minicov_alloc_zeroed(_size: usize, _align: usize) -> *mut u8 {
    ptr::null_mut()
}

/// Stub deallocator when `alloc` feature is disabled — no-op.
///
/// # Safety
///
/// Safe to call; does nothing.
#[cfg(not(feature = "alloc"))]
#[unsafe(no_mangle)]
pub unsafe extern "C" fn minicov_dealloc(_ptr: *mut u8, _size: usize, _align: usize) {}

// === #[unsafe(no_mangle)] section accessor exports ===

/// Returns the start of the profiling counters section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_begin_counters() -> *const u8 {
    platform::begin_counters()
}

/// Returns the end of the profiling counters section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_end_counters() -> *const u8 {
    platform::end_counters()
}

/// Returns the start of the profiling data section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_begin_data() -> *const LlvmProfileData {
    platform::begin_data()
}

/// Returns the end of the profiling data section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_end_data() -> *const LlvmProfileData {
    platform::end_data()
}

/// Returns the start of the profiling names section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_begin_names() -> *const u8 {
    platform::begin_names()
}

/// Returns the end of the profiling names section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_end_names() -> *const u8 {
    platform::end_names()
}

/// Returns the start of the profiling bitmap section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_begin_bitmap() -> *const u8 {
    platform::begin_bitmap()
}

/// Returns the end of the profiling bitmap section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_end_bitmap() -> *const u8 {
    platform::end_bitmap()
}

/// Returns the start of the profiling value nodes section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_begin_vnodes() -> *const ValueProfNode {
    platform::begin_vnodes()
}

/// Returns the end of the profiling value nodes section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_end_vnodes() -> *const ValueProfNode {
    platform::end_vnodes()
}

/// Returns the start of the vtables section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_begin_vtables() -> *const u8 {
    platform::begin_vtables()
}

/// Returns the end of the vtables section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_end_vtables() -> *const u8 {
    platform::end_vtables()
}

/// Returns the start of the vtabnames section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_begin_vtabnames() -> *const u8 {
    platform::begin_vtabnames()
}

/// Returns the end of the vtabnames section.
///
/// # Safety
///
/// Must be called after linker has resolved section boundary symbols.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_end_vtabnames() -> *const u8 {
    platform::end_vtabnames()
}

// === #[unsafe(no_mangle)] profiling operation exports ===

/// Resets all profiling counters to their initial state.
///
/// # Safety
///
/// Must not be called concurrently with other profiling operations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_reset_counters() {
    // SAFETY: forwards the runtime's synchronization precondition unchanged.
    unsafe { profiling::reset_counters() };
}

/// Merges profile data from a buffer into the current counters.
///
/// # Safety
///
/// `profile` must point to a valid, compatible profile buffer of `size` bytes.
/// Must not be called concurrently with other profiling operations.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_merge_from_buffer(profile: *const u8, size: u64) -> i32 {
    // SAFETY: forwards the caller-provided profile buffer contract unchanged.
    unsafe { merge::merge_from_buffer(profile, size) }
}

/// Checks if the given profile data is compatible with the current binary.
///
/// # Safety
///
/// `profile` must point to a buffer of at least `size` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_check_compatibility(profile: *const u8, size: u64) -> i32 {
    // SAFETY: forwards the caller-provided profile buffer contract unchanged.
    unsafe { merge::check_compatibility(profile, size) }
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_get_version() -> u64 {
    profiling::get_version()
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_get_magic() -> u64 {
    profiling::get_magic()
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_get_num_padding_bytes(size_in_bytes: u64) -> u8 {
    types::get_num_padding_bytes(size_in_bytes)
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_get_size_for_buffer() -> u64 {
    buffer::get_size_for_buffer()
}

/// Writes the raw profile data into the provided buffer.
///
/// # Safety
///
/// `buffer` must point to a writable buffer of at least
/// `__llvm_profile_get_size_for_buffer()` bytes.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_write_buffer(buffer: *mut u8) -> i32 {
    // SAFETY: forwards the caller-provided output buffer contract unchanged.
    unsafe { writer::write_buffer(buffer) }
}

/// Records an indirect call target for value profiling.
///
/// # Safety
///
/// `data` must point to a valid `LlvmProfileData` entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_instrument_target(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
) {
    // SAFETY: forwards the compiler-provided profiling record contract unchanged.
    unsafe { value::instrument_target(target_value, data, counter_index) };
}

/// Records an indirect call target with an explicit count.
///
/// # Safety
///
/// `data` must point to a valid `LlvmProfileData` entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_instrument_target_value(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
    count_value: u64,
) {
    // SAFETY: forwards the compiler-provided profiling record contract unchanged.
    unsafe { value::instrument_target_value(target_value, data, counter_index, count_value) };
}

/// Records a memory operation size for value profiling.
///
/// # Safety
///
/// `data` must point to a valid `LlvmProfileData` entry.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn __llvm_profile_instrument_memop(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
) {
    // SAFETY: forwards the compiler-provided profiling record contract unchanged.
    unsafe { value::instrument_memop(target_value, data, counter_index) };
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_get_data_size() -> u64 {
    buffer::get_data_size(platform::begin_data(), platform::end_data())
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_get_counters_size() -> u64 {
    buffer::get_counters_size(platform::begin_counters(), platform::end_counters())
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_get_num_data() -> u64 {
    buffer::get_num_data(platform::begin_data(), platform::end_data())
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_get_num_padding_bytes_for_counters() -> u64 {
    let counters_size =
        buffer::get_counters_size(platform::begin_counters(), platform::end_counters());
    types::get_num_padding_bytes(counters_size) as u64
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_is_continuous_mode_enabled() -> i32 {
    0
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_enable_continuous_mode() {}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_disable_continuous_mode() {}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_set_page_size(_ps: u32) {}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_set_dumped() {
    profiling::set_dumped();
}

#[unsafe(no_mangle)]
pub extern "C" fn __llvm_profile_initialize() {}

/// Low-level profile data writer used by the runtime.
///
/// # Safety
///
/// `writer` must point to a valid `ProfDataWriter` with a correct `write_fn` callback.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lprofWriteData(
    writer: *mut ProfDataWriter,
    vp_data_reader: *mut VPDataReaderType,
    skip_name_data_write: i32,
) -> i32 {
    // SAFETY: forwards the caller-provided writer and callback contracts unchanged.
    unsafe { writer::write_data(writer, vp_data_reader, skip_name_data_write) }
}

#[unsafe(no_mangle)]
pub extern "C" fn lprofGetVPDataReader() -> *mut VPDataReaderType {
    value::get_vpdo_data_reader()
}

#[unsafe(no_mangle)]
pub extern "C" fn lprofGetLoadModuleSignature() -> u64 {
    merge::get_load_module_signature()
}

// === Public Rust API (identical to minicov) ===

/// Returns `true` if code coverage instrumentation is enabled.
pub fn coverage_enabled() -> bool {
    let begin = platform::begin_data();
    let end = platform::end_data();
    (end as usize) > (begin as usize)
}

/// Trait for writing coverage data to an arbitrary destination.
pub trait CoverageWriter {
    fn write(&mut self, data: &[u8]) -> Result<(), CoverageWriteError>;
}

/// Error returned when writing coverage data fails.
#[derive(Copy, Clone, Debug)]
pub struct CoverageWriteError;

impl core::fmt::Display for CoverageWriteError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("error while writing coverage data")
    }
}

/// Error returned when merging incompatible coverage data.
#[derive(Copy, Clone, Debug)]
pub struct IncompatibleCoverageData;

impl core::fmt::Display for IncompatibleCoverageData {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.write_str("incompatible coverage data")
    }
}

/// Captures the current code coverage data and writes it to the given writer.
///
/// This function is not thread-safe: concurrent calls or concurrent execution
/// of instrumented code may produce inaccurate coverage data.
pub fn capture_coverage<Writer: CoverageWriter>(
    writer: &mut Writer,
) -> Result<(), CoverageWriteError> {
    let size = buffer::get_size_for_buffer();

    #[cfg(feature = "alloc")]
    {
        extern crate alloc;
        let mut buf = alloc::vec![0u8; size as usize];

        // SAFETY: `buf` is allocated to exactly the required serialized profile size.
        let result = unsafe { writer::write_buffer(buf.as_mut_ptr()) };
        if result != 0 {
            return Err(CoverageWriteError);
        }

        writer.write(&buf)?;
    }

    #[cfg(not(feature = "alloc"))]
    {
        return Err(CoverageWriteError);
    }

    Ok(())
}

/// Merges the given coverage data into the current counters.
///
/// This function is not thread-safe: concurrent calls or concurrent execution
/// of instrumented code may produce inaccurate coverage data.
pub fn merge_coverage(data: &[u8]) -> Result<(), IncompatibleCoverageData> {
    // SAFETY: `data` exposes a stable slice for the duration of the merge call.
    let result = unsafe { merge::merge_from_buffer(data.as_ptr(), data.len() as u64) };
    if result != 0 {
        Err(IncompatibleCoverageData)
    } else {
        Ok(())
    }
}

/// Resets all profiling counters to zero.
///
/// Must not be called concurrently with other profiling operations.
pub fn reset_coverage() {
    // SAFETY: the safe wrapper inherits the documented non-concurrency requirement.
    unsafe { profiling::reset_counters() };
}

/// Returns a signature value unique to the current load module.
///
/// Must not be called concurrently with other profiling operations.
pub fn module_signature() -> u64 {
    merge::get_load_module_signature()
}

#[cfg(feature = "alloc")]
extern crate alloc;

#[cfg(feature = "alloc")]
impl CoverageWriter for alloc::vec::Vec<u8> {
    fn write(&mut self, data: &[u8]) -> Result<(), CoverageWriteError> {
        self.extend_from_slice(data);
        Ok(())
    }
}
