// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Buffer size calculation for profile data.

use core::mem::size_of;

use crate::{platform, profiling, types::*};

/// Returns the total size needed for a raw profile buffer.
pub fn get_size_for_buffer() -> u64 {
    get_size_for_buffer_internal(
        platform::begin_data(),
        platform::end_data(),
        platform::begin_counters(),
        platform::end_counters(),
        platform::begin_bitmap(),
        platform::end_bitmap(),
        platform::begin_names(),
        platform::end_names(),
        platform::begin_vtables(),
        platform::end_vtables(),
        platform::begin_vtabnames(),
        platform::end_vtabnames(),
    )
}

/// Returns the number of `LlvmProfileData` entries in the range.
pub fn get_num_data(begin: *const LlvmProfileData, end: *const LlvmProfileData) -> u64 {
    div_ceil_u64(
        ptr_range_len(begin.cast::<u8>(), end.cast::<u8>()),
        size_of::<LlvmProfileData>() as u64,
    )
}

/// Returns the data section size in bytes.
pub fn get_data_size(begin: *const LlvmProfileData, end: *const LlvmProfileData) -> u64 {
    get_num_data(begin, end)
        .checked_mul(size_of::<LlvmProfileData>() as u64)
        .expect("profile data size overflow")
}

/// Returns the number of counter entries in the range.
pub fn get_num_counters(begin: *const u8, end: *const u8) -> u64 {
    let entry_sz = crate::port::counter_entry_size(profiling::get_version()) as u64;
    assert!(entry_sz != 0, "counter entry size must be non-zero");
    div_ceil_u64(ptr_range_len(begin, end), entry_sz)
}

/// Returns the counter section size in bytes.
pub fn get_counters_size(begin: *const u8, end: *const u8) -> u64 {
    let entry_sz = crate::port::counter_entry_size(profiling::get_version()) as u64;
    get_num_counters(begin, end)
        .checked_mul(entry_sz)
        .expect("profile counter size overflow")
}

/// Returns the bitmap section size in bytes.
pub fn get_num_bitmap_bytes(begin: *const u8, end: *const u8) -> u64 {
    (end as usize).saturating_sub(begin as usize) as u64
}

/// Returns the names section size in bytes.
pub fn get_name_size(begin: *const u8, end: *const u8) -> u64 {
    (end as usize).saturating_sub(begin as usize) as u64
}

/// Error returned when padding calculation fails (vtables in continuous mode).
#[derive(Copy, Clone, Debug)]
pub struct PaddingError;

/// Calculates padding sizes for counter/bitmap/name sections.
pub fn get_padding_sizes_for_counters(
    data_size: u64,
    counters_size: u64,
    num_bitmap_bytes: u64,
    names_size: u64,
    vtable_size: u64,
    vname_size: u64,
) -> Result<PaddingSizes, PaddingError> {
    if !needs_counter_padding() {
        return Ok(PaddingSizes {
            before_counters: 0,
            after_counters: get_num_padding_bytes(counters_size),
            after_bitmap: get_num_padding_bytes(num_bitmap_bytes),
            after_names: get_num_padding_bytes(names_size),
            after_vtable: get_num_padding_bytes(vtable_size),
            after_vname: get_num_padding_bytes(vname_size),
        });
    }

    if vtable_size != 0 || vname_size != 0 {
        return Err(PaddingError);
    }

    let page_size = crate::port::get_page_size() as u64;
    Ok(PaddingSizes {
        before_counters: crate::port::calculate_bytes_needed_to_page_align(
            checked_add_size(size_of::<LlvmProfileHeader>() as u64, data_size),
            page_size,
        ),
        after_counters: crate::port::calculate_bytes_needed_to_page_align(counters_size, page_size),
        after_bitmap: crate::port::calculate_bytes_needed_to_page_align(
            num_bitmap_bytes,
            page_size,
        ),
        after_names: crate::port::calculate_bytes_needed_to_page_align(names_size, page_size),
        after_vtable: 0,
        after_vname: 0,
    })
}

pub struct PaddingSizes {
    pub before_counters: u64,
    pub after_counters: u64,
    pub after_bitmap: u64,
    pub after_names: u64,
    pub after_vtable: u64,
    pub after_vname: u64,
}

fn needs_counter_padding() -> bool {
    false
}

#[allow(clippy::too_many_arguments)]
fn get_size_for_buffer_internal(
    data_begin: *const LlvmProfileData,
    data_end: *const LlvmProfileData,
    counters_begin: *const u8,
    counters_end: *const u8,
    bitmap_begin: *const u8,
    bitmap_end: *const u8,
    names_begin: *const u8,
    names_end: *const u8,
    _vtable_begin: *const u8,
    _vtable_end: *const u8,
    _vnames_begin: *const u8,
    _vnames_end: *const u8,
) -> u64 {
    let names_size = get_name_size(names_begin, names_end);
    let data_size = get_data_size(data_begin, data_end);
    let counters_size = get_counters_size(counters_begin, counters_end);
    let num_bitmap_bytes = get_num_bitmap_bytes(bitmap_begin, bitmap_end);

    let padding = get_padding_sizes_for_counters(
        data_size,
        counters_size,
        num_bitmap_bytes,
        names_size,
        0,
        0,
    )
    .expect("padding calculation should not fail without vtables");

    let binary_ids_size = platform::get_binary_ids_size();

    [
        size_of::<LlvmProfileHeader>() as u64,
        binary_ids_size,
        data_size,
        padding.before_counters,
        counters_size,
        padding.after_counters,
        num_bitmap_bytes,
        padding.after_bitmap,
        names_size,
        padding.after_names,
    ]
    .into_iter()
    .try_fold(0u64, |total, size| total.checked_add(size))
    .expect("profile buffer size overflow")
}

fn ptr_range_len(begin: *const u8, end: *const u8) -> u64 {
    (end as usize)
        .checked_sub(begin as usize)
        .expect("profile section end precedes begin") as u64
}

fn div_ceil_u64(value: u64, divisor: u64) -> u64 {
    debug_assert!(divisor != 0, "division by zero in profile size calculation");
    value
        .checked_add(divisor - 1)
        .expect("profile section size overflow")
        / divisor
}

fn checked_add_size(lhs: u64, rhs: u64) -> u64 {
    lhs.checked_add(rhs).expect("profile buffer size overflow")
}

fn get_num_padding_bytes(size_bytes: u64) -> u64 {
    get_padding_bytes(size_bytes) as u64
}

fn get_padding_bytes(size: u64) -> u8 {
    (7 & (8 - size % 8)) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn padding_bytes_alignment() {
        assert_eq!(get_num_padding_bytes(0), 0);
        assert_eq!(get_num_padding_bytes(8), 0);
        assert_eq!(get_num_padding_bytes(1), 7);
        assert_eq!(get_num_padding_bytes(7), 1);
        assert_eq!(get_num_padding_bytes(16), 0);
    }

    #[test]
    fn num_data_calculation() {
        let begin = 0x1000 as *const LlvmProfileData;
        let end = 0x1000 as *const LlvmProfileData;
        assert_eq!(get_num_data(begin, end), 0);
    }

    #[test]
    fn padding_sizes_no_continuous_mode() {
        let sizes = get_padding_sizes_for_counters(100, 200, 0, 50, 0, 0);
        assert!(sizes.is_ok());
        let s = sizes.unwrap();
        assert_eq!(s.before_counters, 0);
        assert_eq!(s.after_counters, 0);
        assert_eq!(s.after_names, 6);
    }
}
