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
