// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Profile data merging.

use core::{
    mem::{align_of, size_of},
    ptr,
};

use crate::{buffer, platform, profiling, types::*};

/// Checks whether the given profile data is compatible with the current binary.
/// Mirrors __llvm_profile_check_compatibility in InstrProfilingMerge.c.
///
/// # Safety
///
/// `profile_data` must point to a buffer of at least `profile_size` bytes.
pub unsafe fn check_compatibility(profile_data: *const u8, profile_size: u64) -> i32 {
    // SAFETY: the caller provides a readable profile buffer of `profile_size`
    // bytes, and this routine bounds-checks every typed access within it.
    unsafe {
        if profile_size < size_of::<LlvmProfileHeader>() as u64 {
            return -1;
        }

        let header = if (profile_data as usize).is_multiple_of(align_of::<LlvmProfileHeader>()) {
            ptr::read(profile_data as *const LlvmProfileHeader)
        } else {
            ptr::read_unaligned(profile_data as *const LlvmProfileHeader)
        };

        if header.magic != profiling::get_magic()
            || header.version != profiling::get_version()
            || header.num_data != buffer::get_num_data(platform::begin_data(), platform::end_data())
            || header.num_counters
                != buffer::get_num_counters(platform::begin_counters(), platform::end_counters())
            || header.num_bitmap_bytes
                != buffer::get_num_bitmap_bytes(platform::begin_bitmap(), platform::end_bitmap())
            || header.names_size
                != buffer::get_name_size(platform::begin_names(), platform::end_names())
            || header.value_kind_last != IPVK_LAST as u64
        {
            return -1;
        }

        let entry_size = crate::port::counter_entry_size(profiling::get_version()) as u64;
        if profile_size
            < size_of::<LlvmProfileHeader>() as u64
                + header.binary_ids_size
                + header.num_data * size_of::<LlvmProfileData>() as u64
                + header.names_size
                + header.num_counters * entry_size
                + header.num_bitmap_bytes
        {
            return -1;
        }

        // Per-function matching.
        let src_data_start = profile_data
            .add(size_of::<LlvmProfileHeader>())
            .add(header.binary_ids_size as usize)
            as *const LlvmProfileData;
        let dst_data = platform::begin_data();
        for i in 0..header.num_data as usize {
            let src = src_data_start.add(i);
            let dst = dst_data.add(i);
            if (*src).name_ref != (*dst).name_ref
                || (*src).func_hash != (*dst).func_hash
                || (*src).num_counters != (*dst).num_counters
                || (*src).num_bitmap_bytes != (*dst).num_bitmap_bytes
            {
                return -1;
            }
        }

        0
    }
}

/// Merges profile data from the given buffer into the current counters.
///
/// # Safety
///
/// `profile_data` must point to a compatible profile buffer of `profile_size` bytes.
pub unsafe fn merge_from_buffer(profile_data: *const u8, profile_size: u64) -> i32 {
    // SAFETY: the caller provides a readable compatible profile buffer, and the
    // routine only mutates the in-memory profiling sections owned by this runtime.
    unsafe {
        if check_compatibility(profile_data, profile_size) != 0 {
            return -1;
        }

        let header = profile_data as *const LlvmProfileHeader;
        let counters_begin = platform::begin_counters() as *mut u8;
        let bitmap_begin = platform::begin_bitmap() as *mut u8;

        let src_data = profile_data.add(size_of::<LlvmProfileHeader>());
        let src_data_start = src_data.add((*header).binary_ids_size as usize);
        let src_data_end =
            src_data_start.add((*header).num_data as usize * size_of::<LlvmProfileData>());
        let src_counters_start = src_data_end.add((*header).padding_bytes_before_counters as usize);
        let entry_size = crate::port::counter_entry_size(profiling::get_version());
        let src_counters_end = src_counters_start.add((*header).num_counters as usize * entry_size);

        let is_byte_coverage = profiling::get_version() & VARIANT_MASK_BYTE_COVERAGE != 0;
        let num_counters = (*header).num_counters as usize;

        if is_byte_coverage {
            for i in 0..num_counters {
                let dst = counters_begin.add(i);
                let src = src_counters_start.add(i);
                *dst &= *src;
            }
        } else {
            for i in 0..num_counters {
                let dst = counters_begin.add(i * entry_size) as *mut u64;
                let src = src_counters_start.add(i * entry_size) as *const u64;
                *dst = (*dst).saturating_add(*src);
            }
        }

        let src_bitmap_start =
            src_counters_end.add((*header).padding_bytes_after_counters as usize);
        let num_bitmap_bytes = (*header).num_bitmap_bytes as usize;

        for i in 0..num_bitmap_bytes {
            let dst = bitmap_begin.add(i);
            let src = src_bitmap_start.add(i);
            *dst |= *src;
        }

        0
    }
}

/// Computes a signature for the current load module.
/// Mirrors lprofGetLoadModuleSignature in InstrProfilingMerge.c.
pub fn get_load_module_signature() -> u64 {
    // SAFETY: the profiling section boundary helpers return process-lifetime
    // storage owned by this runtime, and the computation is read-only.
    unsafe {
        let version = profiling::get_version();
        let num_counters =
            buffer::get_num_counters(platform::begin_counters(), platform::end_counters());
        let num_data = buffer::get_num_data(platform::begin_data(), platform::end_data());
        let names_size = buffer::get_name_size(platform::begin_names(), platform::end_names());
        let num_vnodes = (platform::end_vnodes() as usize)
            .saturating_sub(platform::begin_vnodes() as usize)
            / size_of::<ValueProfNode>();
        let data_begin = platform::begin_data();
        let first_name_ref = if num_data > 0 {
            (*data_begin).name_ref
        } else {
            0
        };

        (names_size << 40)
            .wrapping_add(num_counters << 30)
            .wrapping_add(num_data << 20)
            .wrapping_add((num_vnodes as u64) << 10)
            .wrapping_add(first_name_ref)
            .wrapping_add(version)
            .wrapping_add(profiling::get_magic())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_compatibility_with_invalid_data() {
        let buf = [0u8; 256];
        let result = unsafe { check_compatibility(buf.as_ptr(), buf.len() as u64) };
        assert_eq!(result, -1);
    }

    #[test]
    fn check_compatibility_with_small_buffer() {
        let buf = [0u8; 4];
        let result = unsafe { check_compatibility(buf.as_ptr(), buf.len() as u64) };
        assert_eq!(result, -1);
    }
}

#[cfg(unittest)]
mod unittest_tests {
    use alloc::vec::Vec;
    use core::{mem::size_of, slice};

    use unittest::{assert, assert_eq, def_test};

    use super::*;

    unsafe fn build_compatible_profile_blob() -> Vec<u8> {
        let num_data = buffer::get_num_data(platform::begin_data(), platform::end_data());
        let num_counters =
            buffer::get_num_counters(platform::begin_counters(), platform::end_counters());
        let num_bitmap_bytes =
            buffer::get_num_bitmap_bytes(platform::begin_bitmap(), platform::end_bitmap());
        let names_size = buffer::get_name_size(platform::begin_names(), platform::end_names());
        let entry_size = crate::port::counter_entry_size(profiling::get_version());

        let header = LlvmProfileHeader {
            magic: profiling::get_magic(),
            version: profiling::get_version(),
            binary_ids_size: 0,
            num_data,
            padding_bytes_before_counters: 0,
            num_counters,
            padding_bytes_after_counters: 0,
            num_bitmap_bytes,
            padding_bytes_after_bitmap_bytes: 0,
            names_size,
            counters_delta: 0,
            bitmap_delta: 0,
            names_delta: 0,
            num_vtables: 0,
            vnames_size: 0,
            value_kind_last: IPVK_LAST as u64,
        };

        let mut blob = Vec::with_capacity(
            size_of::<LlvmProfileHeader>()
                + num_data as usize * size_of::<LlvmProfileData>()
                + num_counters as usize * entry_size
                + num_bitmap_bytes as usize
                + names_size as usize,
        );
        // SAFETY: `&header` references a fully-initialized local `LlvmProfileHeader`
        // on the stack; the derived `*const u8` is aligned to `align_of::<u8>() == 1`
        // and the byte length equals `size_of::<LlvmProfileHeader>()`, so the slice
        // covers exactly the header's initialized bytes.
        blob.extend_from_slice(unsafe {
            slice::from_raw_parts(
                (&header as *const LlvmProfileHeader).cast::<u8>(),
                size_of::<LlvmProfileHeader>(),
            )
        });
        // SAFETY: `begin_data()`/`end_data()` bound the linker-provided
        // `__llvm_prf_data` section (process-lifetime, properly aligned for
        // `LlvmProfileData`), and `num_data` is derived from that same range via
        // `get_num_data`, so `num_data * size_of::<LlvmProfileData>()` bytes are
        // valid and readable.
        blob.extend_from_slice(unsafe {
            slice::from_raw_parts(
                platform::begin_data().cast::<u8>(),
                num_data as usize * size_of::<LlvmProfileData>(),
            )
        });
        // SAFETY: `begin_counters()` points at the `__llvm_prf_cnts` section
        // (process-lifetime storage), and `num_counters * entry_size` is the exact
        // byte length that `get_num_counters` computes over that same section, so
        // the slice stays within initialized section memory.
        blob.extend_from_slice(unsafe {
            slice::from_raw_parts(
                platform::begin_counters(),
                num_counters as usize * entry_size,
            )
        });
        // SAFETY: `begin_bitmap()` points at the `__llvm_prf_bits` section
        // (process-lifetime storage), and `num_bitmap_bytes` is the byte length
        // `get_num_bitmap_bytes` derives from `begin_bitmap()`/`end_bitmap()`, so
        // the slice stays within the section.
        blob.extend_from_slice(unsafe {
            slice::from_raw_parts(platform::begin_bitmap(), num_bitmap_bytes as usize)
        });
        // SAFETY: `begin_names()` points at the `__llvm_prf_names` section
        // (process-lifetime storage), and `names_size` is the byte length
        // `get_name_size` derives from `begin_names()`/`end_names()`, so the slice
        // stays within the section.
        blob.extend_from_slice(unsafe {
            slice::from_raw_parts(platform::begin_names(), names_size as usize)
        });
        blob
    }

    #[def_test(serial)]
    fn compatibility_accepts_runtime_snapshot() {
        // SAFETY: `build_compatible_profile_blob()` only reads from the
        // process-lifetime profiling sections and a stack `LlvmProfileHeader`;
        // it performs no mutation of shared state.
        let blob = unsafe { build_compatible_profile_blob() };
        // SAFETY: `blob` is a `Vec<u8>` owned for the call's duration, so
        // `blob.as_ptr()` is valid and `blob.len()` is the exact readable byte
        // count, satisfying `check_compatibility`'s buffer contract.
        let result = unsafe { check_compatibility(blob.as_ptr(), blob.len() as u64) };
        assert_eq!(result, 0);
    }

    #[def_test(serial)]
    fn compatibility_rejects_mismatched_function_metadata() {
        // SAFETY: `build_compatible_profile_blob()` only reads from the
        // process-lifetime profiling sections and a stack `LlvmProfileHeader`.
        let mut blob = unsafe { build_compatible_profile_blob() };
        let data_offset = size_of::<LlvmProfileHeader>();
        // SAFETY: `blob` was built with capacity
        // `size_of::<LlvmProfileHeader>() + num_data * size_of::<LlvmProfileData>() + ...`,
        // and is fully initialized, so `data_offset == size_of::<LlvmProfileHeader>()`
        // lies within the allocation; the blob's bytes are `u8`-aligned so `add`
        // stays in-bounds.
        let data_ptr = unsafe { blob.as_mut_ptr().add(data_offset) }.cast::<LlvmProfileData>();
        // SAFETY: `data_ptr` points at the first `LlvmProfileData` inside the blob,
        // which is initialized from the `__llvm_prf_data` section; the section is
        // 8-byte aligned (INSTR_PROF_DATA_ALIGNMENT) and the blob preserves that
        // alignment because the header is exactly 128 bytes, so the dereference is
        // aligned and reads initialized memory. `blob` is exclusively borrowed via
        // `as_mut_ptr`, so there is no aliasing.
        unsafe {
            (*data_ptr).func_hash ^= 1;
        }

        // SAFETY: `blob` is still owned and valid; `blob.as_ptr()`/`blob.len()`
        // satisfy `check_compatibility`'s readable-buffer contract.
        let result = unsafe { check_compatibility(blob.as_ptr(), blob.len() as u64) };
        assert_eq!(result, -1);
    }

    #[def_test(serial)]
    fn merge_from_buffer_updates_counters_and_bitmap() {
        // SAFETY: `build_compatible_profile_blob()` only reads from the
        // process-lifetime profiling sections and a stack `LlvmProfileHeader`.
        let mut blob = unsafe { build_compatible_profile_blob() };
        // SAFETY: `blob` starts with a fully initialized `LlvmProfileHeader`
        // (128 bytes, 8-byte aligned) written by `build_compatible_profile_blob`,
        // so the cast and shared reference are aligned and read initialized bytes;
        // no mutable borrow of `blob` is outstanding.
        let header = unsafe { &*(blob.as_ptr().cast::<LlvmProfileHeader>()) };
        let entry_size = crate::port::counter_entry_size(profiling::get_version());
        assert!(header.num_counters > 0);

        let counters_offset = size_of::<LlvmProfileHeader>()
            + header.num_data as usize * size_of::<LlvmProfileData>();
        let bitmap_offset = counters_offset + header.num_counters as usize * entry_size;
        let byte_coverage = profiling::get_version() & VARIANT_MASK_BYTE_COVERAGE != 0;

        let counters_begin = platform::begin_counters() as *mut u8;
        let bitmap_begin = platform::begin_bitmap() as *mut u8;

        if byte_coverage {
            // SAFETY: `counters_begin` is `begin_counters()`, the process-lifetime
            // `__llvm_prf_cnts` section base; `header.num_counters > 0` guarantees
            // at least one byte (byte-coverage mode uses 1-byte entries), so the
            // read is in-bounds and the section is zero-initialized.
            let original = unsafe { *counters_begin };
            blob[counters_offset] = 0;
            // SAFETY: `blob` is owned and valid for `blob.len()` bytes (satisfying
            // `merge_from_buffer`'s contract); `counters_begin` is the writable
            // process-lifetime counters section, and these tests are `#[serial]`
            // so no other thread accesses the counters concurrently.
            unsafe {
                *counters_begin = 0xFF;
                let result = merge_from_buffer(blob.as_ptr(), blob.len() as u64);
                assert_eq!(result, 0);
                assert_eq!(*counters_begin, 0);
                *counters_begin = original;
            }
        } else {
            // SAFETY: `counters_begin` is the `__llvm_prf_cnts` section base; the
            // section is 8-byte aligned and `header.num_counters > 0` means at least
            // one `u64`-sized entry exists, so the cast and read are aligned and the
            // section is zero-initialized.
            let original = unsafe { *(counters_begin.cast::<u64>()) };
            let merged = 7u64;
            blob[counters_offset..counters_offset + size_of::<u64>()]
                .copy_from_slice(&merged.to_le_bytes());
            // SAFETY: `blob` is owned and valid for `blob.len()` bytes; the counters
            // section is writable process-lifetime storage, and the test is `#[serial]`
            // so access to `counters_begin` is exclusive.
            unsafe {
                let result = merge_from_buffer(blob.as_ptr(), blob.len() as u64);
                assert_eq!(result, 0);
                assert_eq!(
                    *(counters_begin.cast::<u64>()),
                    original.saturating_add(merged)
                );
                *(counters_begin.cast::<u64>()) = original;
            }
        }

        if header.num_bitmap_bytes > 0 {
            // SAFETY: `bitmap_begin` is `begin_bitmap()`, the process-lifetime
            // `__llvm_prf_bits` section base; `header.num_bitmap_bytes > 0`
            // guarantees at least one byte is present, so the read is in-bounds and
            // the section is zero-initialized.
            let original = unsafe { *bitmap_begin };
            blob[bitmap_offset] = 0x5A;
            // SAFETY: `blob` is owned and valid for `blob.len()` bytes; `bitmap_begin`
            // is the writable process-lifetime bitmap section, and the test is
            // `#[serial]` so access is exclusive.
            unsafe {
                let result = merge_from_buffer(blob.as_ptr(), blob.len() as u64);
                assert_eq!(result, 0);
                assert_eq!(*bitmap_begin, original | 0x5A);
                *bitmap_begin = original;
            }
        }
    }
}
