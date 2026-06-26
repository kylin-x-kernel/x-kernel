// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Value profiling — safe implementation using `Vec<ValueCount>`.
//!
//! No `unsafe`, no C linked lists, no raw pointers, no `Box::from_raw`.
//! The core logic delegates to `ValueProfileStore::record_value`.
//! ABI callbacks in `abi::exports` call through `runtime::record_value`.

use core::ffi::c_void;

use crate::{abi::layout::LlvmProfileData, platform, runtime};

/// Resolves a function index from a `LlvmProfileData` pointer.
///
/// LLVM passes the address of the function's `LlvmProfileData` record in the
/// `__llvm_prf_data` linker section. By computing the offset from the section
/// start, we can determine which function is being instrumented.
///
/// Returns `None` if the pointer is outside the data section or misaligned.
fn resolve_function_index(data: *const LlvmProfileData) -> Option<usize> {
    if data.is_null() {
        return None;
    }

    let sections = platform::profile_sections();
    let data_begin = sections.data.begin();
    let data_end = sections.data.end();

    // Pointer must be within the data section.
    if data < data_begin || data >= data_end {
        return None;
    }

    // Pointer must be properly aligned.
    let byte_offset = (data as usize).saturating_sub(data_begin as usize);
    let record_size = core::mem::size_of::<LlvmProfileData>();
    if !byte_offset.is_multiple_of(record_size) {
        return None;
    }

    Some(byte_offset / record_size)
}

/// Computes the flat site index for a given function and counter_index.
///
/// In LLVM's value profiling, `counter_index` is the site index *within* a
/// function for a specific value kind. We need to compute a global flat index
/// by summing all value sites of earlier functions.
///
/// Returns `None` if the function index is out of range or arithmetic overflows.
fn compute_flat_site_index(
    func_index: usize,
    counter_index: u32,
    value_kind: usize,
) -> Option<usize> {
    let sections = platform::profile_sections();
    let data_slice = sections.data.as_slice();

    if func_index >= data_slice.len() {
        return None;
    }

    // Sum all value sites across all kinds for functions before this one.
    let mut flat_offset: usize = 0;
    for record in data_slice.iter().take(func_index) {
        for kind_sites in &record.num_value_sites {
            flat_offset = flat_offset.checked_add(*kind_sites as usize)?;
        }
    }

    // Add sites of earlier kinds within this function.
    let record = &data_slice[func_index];
    for k in 0..value_kind {
        flat_offset = flat_offset.checked_add(record.num_value_sites[k] as usize)?;
    }

    // Add the counter_index within this kind.
    flat_offset.checked_add(counter_index as usize)
}

/// Records a target value for indirect call profiling.
///
/// This is the entry point called from `abi::exports`. It resolves the
/// `LlvmProfileData` pointer to a function index and site index, then
/// delegates to the safe runtime.
pub(crate) fn instrument_target(target_value: u64, data: *mut c_void, counter_index: u32) {
    instrument_target_value(target_value, data, counter_index, 1);
}

/// Records a target value with an explicit count.
pub(crate) fn instrument_target_value(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
    count_value: u64,
) {
    if count_value == 0 {
        return;
    }

    let Some(func_index) = resolve_function_index(data as *const LlvmProfileData) else {
        return;
    };

    // IPVK_INDIRECT_CALL_TARGET = 0
    let Some(flat_site) = compute_flat_site_index(func_index, counter_index, 0) else {
        return;
    };

    runtime::record_value(flat_site, target_value, count_value);
}

/// Records a memory operation size value with log2 bucketing.
pub(crate) fn instrument_memop(target_value: u64, data: *mut c_void, counter_index: u32) {
    let rep_value = get_range_rep_value(target_value);

    let Some(func_index) = resolve_function_index(data as *const LlvmProfileData) else {
        return;
    };

    // IPVK_MEMOP_SIZE = 1
    let Some(flat_site) = compute_flat_site_index(func_index, counter_index, 1) else {
        return;
    };

    runtime::record_value(flat_site, rep_value, 1);
}

/// Maps an observed memop size value to the representative value of its range.
/// Mirrors `InstrProfGetRangeRepValue` in InstrProfData.inc.
pub fn get_range_rep_value(value: u64) -> u64 {
    if value <= 8 {
        return value;
    }
    if value >= 513 {
        return 513;
    }
    if value.count_ones() == 1 {
        return value;
    }
    (1u64 << (64 - value.leading_zeros() as u64 - 1)) + 1
}
