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

// SAFETY: LLVM instrumentation probes this unique C int marker by its exact name;
// the i32 layout matches compiler-rt's `__llvm_profile_runtime` definition.
#[unsafe(no_mangle)]
pub(crate) static __llvm_profile_runtime: i32 = 0;

// SAFETY: LLVM reads this unique raw-format version symbol as a 64-bit integer; its value comes
// from the matching layout constant.
#[unsafe(no_mangle)]
pub(crate) static __llvm_profile_raw_version: u64 = INSTR_PROF_RAW_VERSION;

// === #[unsafe(no_mangle)] section accessor exports ===

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined counters
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the first counter byte
// in the linker-provided section.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_counters() -> *const u8 {
    platform::profile_sections().counters.begin()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined counters
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the one-past-end
// counter byte.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_counters() -> *const u8 {
    platform::profile_sections().counters.end()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined data
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the first
// LlvmProfileData record.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_data() -> *const LlvmProfileData {
    platform::profile_sections().data.begin()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined data
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the one-past-end
// LlvmProfileData record.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_data() -> *const LlvmProfileData {
    platform::profile_sections().data.end()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined names
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the first profile-name
// byte.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_names() -> *const u8 {
    platform::profile_sections().names.begin()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined names
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the one-past-end
// profile-name byte.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_names() -> *const u8 {
    platform::profile_sections().names.end()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined bitmap
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the first bitmap byte.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_bitmap() -> *const u8 {
    platform::profile_sections().bitmap.begin()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined bitmap
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the one-past-end bitmap
// byte.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_bitmap() -> *const u8 {
    platform::profile_sections().bitmap.end()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined vnodes
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the first value-profile
// node.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_vnodes() -> *const ValueProfNode {
    platform::profile_sections().vnodes.begin()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined vnodes
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the one-past-end
// value-profile node.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_vnodes() -> *const ValueProfNode {
    platform::profile_sections().vnodes.end()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined vtables
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the first vtable byte.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_vtables() -> *const u8 {
    platform::profile_sections().vtables.begin()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined vtables
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the one-past-end vtable
// byte.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_vtables() -> *const u8 {
    platform::profile_sections().vtables.end()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined vtabnames
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the first vtable-name byte.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_begin_vtabnames() -> *const u8 {
    platform::profile_sections().vtabnames.begin()
}

/// # Safety
///
/// No caller preconditions: this only returns the linker-defined vtabnames
/// section boundary. Dereferencing the returned pointer requires a separate
/// validity check; an end pointer must not be dereferenced.
// SAFETY: LLVM calls this exact C ABI accessor with no arguments to get the one-past-end
// vtable-name byte.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_end_vtabnames() -> *const u8 {
    platform::profile_sections().vtabnames.end()
}

// === #[unsafe(no_mangle)] profiling operation exports ===

// SAFETY: LLVM runtime callers use this unique no-argument C ABI hook to reset this image’s
// counters.
/// Resets all profiling counters to their initial state.
///
/// # Safety
///
/// No caller preconditions: the runtime resets initialized profiling state
/// through atomic stores and handles an uninitialized image as a no-op.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_reset_counters() {
    crate::reset();
}

// SAFETY: LLVM runtime callers pass a profile pointer and u64 byte count to this unique C ABI
// merge hook; its i32 result reports success or failure.
/// Merges profile data from a buffer into the current counters.
///
/// # Safety
///
/// For non-null `profile`, `size` must describe initialized, readable bytes
/// within one allocation, with length at most `isize::MAX` and no address
/// wraparound. The bytes must remain immutable for this call. A null pointer
/// is accepted and checked without dereferencing it.
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

// SAFETY: LLVM runtime callers pass a profile pointer and u64 byte count to this unique C ABI
// compatibility hook; its i32 result reports compatibility.
/// Checks if the given profile data is compatible with the current binary.
///
/// # Safety
///
/// For non-null `profile`, `size` must describe initialized, readable bytes
/// within one allocation, with length at most `isize::MAX` and no address
/// wraparound. The bytes must remain immutable for this call. A null pointer
/// is accepted and checked without dereferencing it.
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

// SAFETY: LLVM runtime callers use this unique no-argument C ABI getter for the u64 raw-format
// version.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_version() -> u64 {
    INSTR_PROF_RAW_VERSION
}

// SAFETY: LLVM runtime callers use this unique no-argument C ABI getter for the
// pointer-width-specific u64 file magic.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_magic() -> u64 {
    if core::mem::size_of::<*const c_void>() == 8 {
        INSTR_PROF_RAW_MAGIC_64
    } else {
        INSTR_PROF_RAW_MAGIC_32
    }
}

// SAFETY: LLVM runtime callers pass a u64 byte count to this unique C ABI helper and expect a
// u8 padding count.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_num_padding_bytes(size_in_bytes: u64) -> u8 {
    get_num_padding_bytes(size_in_bytes)
}

// SAFETY: LLVM runtime callers use this unique no-argument C ABI getter for the u64
// encoded-buffer size.
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

// SAFETY: LLVM runtime callers pass a writable output pointer to this unique C ABI hook after
// querying the size; i32 reports the write result.
/// Writes the raw profile data into the provided buffer.
///
/// # Safety
///
/// When profiling output is available and `buffer` is non-null, it must
/// point into one allocation containing enough initialized, exclusively
/// writable bytes for the current encoded snapshot, with length at most
/// `isize::MAX`. Keep it live throughout this call. If sizing with
/// `__llvm_profile_get_size_for_buffer`, prevent profile growth between
/// sizing and writing so the allocated capacity remains sufficient.
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

// SAFETY: LLVM-generated indirect-call probes call this unique C ABI hook with a u64 target,
// data pointer, and u32 counter index.
/// Records an indirect call target for value profiling.
///
/// # Safety
///
/// No caller preconditions: `data` is used only as an address to resolve a
/// record in the linker-owned profile section, never dereferenced directly.
/// The value-profiling runtime validates indices and synchronizes updates.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_instrument_target(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
) {
    value::instrument_target(target_value, data, counter_index);
}

// SAFETY: LLVM-generated counted-target probes call this unique C ABI hook with a u64 target,
// data pointer, u32 index, and u64 count.
/// Records an indirect call target with an explicit count.
///
/// # Safety
///
/// No caller preconditions: `data` is used only as an address to resolve a
/// record in the linker-owned profile section, never dereferenced directly.
/// The value-profiling runtime validates indices and synchronizes updates.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_instrument_target_value(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
    count_value: u64,
) {
    value::instrument_target_value(target_value, data, counter_index, count_value);
}

// SAFETY: LLVM-generated memory-size probes call this unique C ABI hook with a u64 value, data
// pointer, and u32 counter index.
/// Records a memory operation size for value profiling.
///
/// # Safety
///
/// No caller preconditions: `data` is used only as an address to resolve a
/// record in the linker-owned profile section, never dereferenced directly.
/// The value-profiling runtime validates indices and synchronizes updates.
#[unsafe(no_mangle)]
pub(crate) unsafe extern "C" fn __llvm_profile_instrument_memop(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
) {
    value::instrument_memop(target_value, data, counter_index);
}

// SAFETY: LLVM runtime callers resolve this unique C ABI getter with Begin/End
// data pointers and expect the whole-record byte size, rounded up, as u64.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_data_size(
    begin: *const LlvmProfileData,
    end: *const LlvmProfileData,
) -> u64 {
    __llvm_profile_get_num_data(begin, end) * core::mem::size_of::<LlvmProfileData>() as u64
}

// SAFETY: LLVM runtime callers resolve this unique C ABI getter with Begin/End
// counter-byte pointers and expect the whole-counter byte size, rounded up, as u64.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_counters_size(begin: *const u8, end: *const u8) -> u64 {
    let entry_size = if INSTR_PROF_RAW_VERSION & VARIANT_MASK_BYTE_COVERAGE != 0 {
        1
    } else {
        core::mem::size_of::<u64>()
    };
    (end as usize)
        .saturating_sub(begin as usize)
        .div_ceil(entry_size) as u64
        * entry_size as u64
}

// SAFETY: LLVM runtime callers resolve this unique C ABI getter with Begin/End
// LlvmProfileData pointers and expect the rounded-up record count as u64.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_num_data(
    begin: *const LlvmProfileData,
    end: *const LlvmProfileData,
) -> u64 {
    (end as usize)
        .saturating_sub(begin as usize)
        .div_ceil(core::mem::size_of::<LlvmProfileData>()) as u64
}

// SAFETY: LLVM runtime callers use this unique no-argument C ABI getter for the u64
// counter-padding count.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_get_num_padding_bytes_for_counters() -> u64 {
    let sections = platform::profile_sections();
    let counters_size = sections.counters.byte_len() as u64;
    get_num_padding_bytes(counters_size) as u64
}

// SAFETY: LLVM runtime callers use this unique no-argument C ABI query; i32 zero declares
// continuous mode disabled.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_is_continuous_mode_enabled() -> i32 {
    0
}

// SAFETY: LLVM runtime callers may resolve this unique no-argument C ABI hook; the no-op
// preserves unsupported continuous mode.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_enable_continuous_mode() {}

// SAFETY: LLVM runtime callers may resolve this unique no-argument C ABI hook; the no-op
// preserves disabled continuous mode.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_disable_continuous_mode() {}

// SAFETY: LLVM runtime callers may pass a u32 page size to this unique C ABI hook; this runtime
// ignores it with continuous mode disabled.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_set_page_size(_ps: u32) {}

// SAFETY: LLVM runtime callers use this unique no-argument C ABI hook to mark the current image
// as dumped.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_set_dumped() {
    crate::state::set_dumped(true);
}

// SAFETY: LLVM runtime callers may resolve this unique no-argument C ABI initialization hook;
// initialization is handled elsewhere.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn __llvm_profile_initialize() {}

// SAFETY: LLVM runtime callers resolve this unique C ABI name and expect a u64 module signature
// for profile merging.
#[unsafe(no_mangle)]
pub(crate) extern "C" fn lprofGetLoadModuleSignature() -> u64 {
    crate::module_signature()
}

#[cfg(test)]
#[path = "../../tests/host/abi_exports.rs"]
mod abi_exports_tests;
