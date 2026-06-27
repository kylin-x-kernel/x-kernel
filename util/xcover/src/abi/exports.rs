// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! LLVM profiling ABI exports.
//!
//! These `#[unsafe(no_mangle)]` symbols are required by LLVM compiler
//! instrumentation. They are `pub(crate)` in Rust visibility — the symbols
//! still appear in the final binary, but are not accessible via the crate's
//! public Rust API.
//!
//! All ABI functions are thin wrappers: validate raw input → convert to
//! structured command → call safe runtime/core → encode return code.

use core::ffi::c_void;

use super::layout::*;
use crate::{platform, value};

// === #[unsafe(no_mangle)] exports required by compiler instrumentation ===

#[unsafe(no_mangle)]
pub(crate) static __llvm_profile_runtime: u8 = 0;

#[unsafe(no_mangle)]
pub(crate) static __llvm_profile_raw_version: u64 = INSTR_PROF_RAW_VERSION;

// === #[unsafe(no_mangle)] section accessor exports ===

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_counters() -> *const u8 {
    platform::profile_sections().counters.begin()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_counters() -> *const u8 {
    platform::profile_sections().counters.end()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_data() -> *const LlvmProfileData {
    platform::profile_sections().data.begin()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_data() -> *const LlvmProfileData {
    platform::profile_sections().data.end()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_names() -> *const u8 {
    platform::profile_sections().names.begin()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_names() -> *const u8 {
    platform::profile_sections().names.end()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_bitmap() -> *const u8 {
    platform::profile_sections().bitmap.begin()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_bitmap() -> *const u8 {
    platform::profile_sections().bitmap.end()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_vnodes() -> *const ValueProfNode {
    platform::profile_sections().vnodes.begin()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_vnodes() -> *const ValueProfNode {
    platform::profile_sections().vnodes.end()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_vtables() -> *const u8 {
    platform::profile_sections().vtables.begin()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_vtables() -> *const u8 {
    platform::profile_sections().vtables.end()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_vtabnames() -> *const u8 {
    platform::profile_sections().vtabnames.begin()
}

#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_vtabnames() -> *const u8 {
    platform::profile_sections().vtabnames.end()
}

// === #[unsafe(no_mangle)] profiling operation exports ===

/// Resets all profiling counters to their initial state.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_reset_counters() {
    crate::reset();
}

/// Merges profile data from a buffer into the current counters.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_merge_from_buffer(
    profile: *const u8,
    size: u64,
) -> i32 {
    // SAFETY: LLVM's merge ABI supplies a readable `profile` buffer for
    // exactly `size` bytes.
    let Some(profile) = (unsafe { AbiProfile::from_raw_parts(profile, size) }) else {
        return -1;
    };
    match crate::merge_profraw(profile.as_bytes()) {
        Ok(()) => 0,
        Err(_) => -1,
    }
}

/// Checks if the given profile data is compatible with the current binary.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_check_compatibility(
    profile: *const u8,
    size: u64,
) -> i32 {
    // SAFETY: LLVM's check ABI supplies a readable `profile` buffer for
    // exactly `size` bytes.
    let Some(profile) = (unsafe { AbiProfile::from_raw_parts(profile, size) }) else {
        return -1;
    };
    match crate::parse::parse_profraw(profile.as_bytes()) {
        Ok(parsed) => match crate::runtime::image_or_init() {
            Ok(image) => match crate::parse::check_compatibility(image, &parsed) {
                Ok(()) => 0,
                Err(_) => -1,
            },
            Err(_) => -1,
        },
        Err(_) => -1,
    }
}

/// Borrowed profile bytes supplied by the LLVM profiling ABI.
struct AbiProfile<'a> {
    bytes: &'a [u8],
}

impl<'a> AbiProfile<'a> {
    /// Converts an ABI pointer and byte length into borrowed profile data.
    ///
    /// Returns `None` when `size` does not fit in `usize`, or when a null
    /// pointer is paired with a nonzero size.
    ///
    /// # Safety
    ///
    /// When `size != 0`, `profile` must point to `size` initialized bytes
    /// that are valid for reads and remain readable for the returned borrow.
    unsafe fn from_raw_parts(profile: *const u8, size: u64) -> Option<Self> {
        let size = usize::try_from(size).ok()?;
        if profile.is_null() {
            return (size == 0).then_some(Self { bytes: &[] });
        }

        // SAFETY: the caller's contract guarantees that `profile` is non-null,
        // points to `size` initialized bytes, and remains readable for `'a`.
        let bytes = unsafe { core::slice::from_raw_parts(profile, size) };
        Some(Self { bytes })
    }

    fn as_bytes(&self) -> &'a [u8] {
        self.bytes
    }
}

/// Writable output buffer supplied by the LLVM profiling ABI.
struct AbiOutputBuffer<'a> {
    bytes: &'a mut [u8],
}

impl<'a> AbiOutputBuffer<'a> {
    /// Converts an ABI pointer and byte length into a mutable output buffer.
    ///
    /// Returns `None` when a null pointer is paired with a nonzero size.
    ///
    /// # Safety
    ///
    /// When `size != 0`, `buffer` must point to `size` initialized bytes
    /// that are valid for writes, uniquely borrowed for the returned borrow,
    /// and remain allocated for that borrow.
    unsafe fn from_raw_parts(buffer: *mut u8, size: usize) -> Option<Self> {
        if buffer.is_null() {
            return (size == 0).then_some(Self { bytes: &mut [] });
        }

        // SAFETY: the caller's contract guarantees that `buffer` is non-null,
        // writable for `size` bytes, uniquely borrowed for `'a`, and remains
        // allocated for that borrow.
        let bytes = unsafe { core::slice::from_raw_parts_mut(buffer, size) };
        Some(Self { bytes })
    }

    fn as_mut_bytes(&mut self) -> &mut [u8] {
        self.bytes
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_version() -> u64 {
    INSTR_PROF_RAW_VERSION
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_magic() -> u64 {
    if core::mem::size_of::<*const c_void>() == 8 {
        INSTR_PROF_RAW_MAGIC_64
    } else {
        INSTR_PROF_RAW_MAGIC_32
    }
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_num_padding_bytes(size_in_bytes: u64) -> u8 {
    get_num_padding_bytes(size_in_bytes)
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_size_for_buffer() -> u64 {
    #[cfg(feature = "alloc")]
    {
        match crate::runtime::snapshot() {
            Some(snap) => crate::serialize::encoded_size(&snap),
            None => 0,
        }
    }
    #[cfg(not(feature = "alloc"))]
    {
        0
    }
}

/// Writes the raw profile data into the provided buffer.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_write_buffer(buffer: *mut u8) -> i32 {
    #[cfg(feature = "alloc")]
    {
        let Some(snapshot) = crate::runtime::snapshot() else {
            return -1;
        };
        let size = crate::serialize::encoded_size(&snapshot);
        let Ok(usize_size) = usize::try_from(size) else {
            return -1;
        };
        // SAFETY: LLVM's buffer-write ABI supplies a writable output buffer
        // of the size previously returned by `__llvm_profile_get_size_for_buffer`.
        let Some(mut buffer) = (unsafe { AbiOutputBuffer::from_raw_parts(buffer, usize_size) })
        else {
            return -1;
        };
        let mut writer = crate::abi::sink::AbiSink::from_buffer(buffer.as_mut_bytes());
        match crate::serialize::encode(&snapshot, &mut writer) {
            Ok(()) => 0,
            Err(_) => -1,
        }
    }
    #[cfg(not(feature = "alloc"))]
    {
        let _ = buffer;
        -1
    }
}

/// Records an indirect call target for value profiling.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_instrument_target(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
) {
    value::instrument_target(target_value, data, counter_index);
}

/// Records an indirect call target with an explicit count.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_instrument_target_value(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
    count_value: u64,
) {
    value::instrument_target_value(target_value, data, counter_index, count_value);
}

/// Records a memory operation size for value profiling.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_instrument_memop(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
) {
    value::instrument_memop(target_value, data, counter_index);
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_data_size() -> u64 {
    let sections = platform::profile_sections();
    sections.data.len() as u64 * core::mem::size_of::<LlvmProfileData>() as u64
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_counters_size() -> u64 {
    let sections = platform::profile_sections();
    sections.counters.byte_len() as u64
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_num_data() -> u64 {
    let sections = platform::profile_sections();
    sections.data.len() as u64
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_num_padding_bytes_for_counters() -> u64 {
    let sections = platform::profile_sections();
    let counters_size = sections.counters.byte_len() as u64;
    get_num_padding_bytes(counters_size) as u64
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_is_continuous_mode_enabled() -> i32 {
    0
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_enable_continuous_mode() {}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_disable_continuous_mode() {}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_set_page_size(_ps: u32) {}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_set_dumped() {
    crate::state::set_dumped(true);
}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_initialize() {}

#[unsafe(no_mangle)]
pub(crate) extern "C" fn lprofGetLoadModuleSignature() -> u64 {
    crate::module_signature()
}
