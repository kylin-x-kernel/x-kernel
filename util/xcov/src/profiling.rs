// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Core profiling functions.

use core::{ffi::c_void, mem::size_of, sync::atomic::Ordering};

use portable_atomic::AtomicU32;

use crate::{internal, platform, port, types::*};

static GLOBAL_TIMESTAMP: AtomicU32 = AtomicU32::new(1);

/// Returns the raw profile magic number for the current pointer width.
pub fn get_magic() -> u64 {
    if size_of::<*const c_void>() == 8 {
        INSTR_PROF_RAW_MAGIC_64
    } else {
        INSTR_PROF_RAW_MAGIC_32
    }
}

/// Returns the profile version with variant flags.
pub fn get_version() -> u64 {
    crate::version::get_raw_version()
}

/// Sets the timestamp probe value if it hasn't been set yet.
///
/// # Safety
///
/// `probe` must point to a valid u64.
pub unsafe fn set_timestamp(probe: *mut u64) {
    // SAFETY: the caller guarantees `probe` points to a valid writable `u64`.
    let val = unsafe { *probe };
    if val == 0 || val == u64::MAX {
        // SAFETY: the caller guarantees `probe` points to a valid writable `u64`.
        unsafe { *probe = GLOBAL_TIMESTAMP.fetch_add(1, Ordering::Relaxed) as u64 };
    }
}

/// Resets all profiling counters, bitmaps, and value profiling data.
///
/// # Safety
///
/// Must be called when no concurrent profiling is happening.
pub unsafe fn reset_counters() {
    // SAFETY: the caller guarantees there is no concurrent profiling activity,
    // so the runtime sections may be reset in place.
    unsafe {
        if get_version() & VARIANT_MASK_TEMPORAL_PROF != 0 {
            GLOBAL_TIMESTAMP.store(1, Ordering::Relaxed);
        }

        let counters_begin = platform::begin_counters() as *mut u8;
        let counters_end = platform::end_counters() as *mut u8;
        let counters_len = counters_end.offset_from(counters_begin) as usize;

        let reset_value: u8 = if get_version() & VARIANT_MASK_BYTE_COVERAGE != 0 {
            0xFF
        } else {
            0
        };
        port::mem_set(counters_begin, reset_value, counters_len);

        let bitmap_begin = platform::begin_bitmap() as *mut u8;
        let bitmap_end = platform::end_bitmap() as *mut u8;
        let bitmap_len = bitmap_end.offset_from(bitmap_begin) as usize;
        port::mem_zero(bitmap_begin, bitmap_len);

        let data_begin = platform::begin_data();
        let data_end = platform::end_data();
        let num_data = ((data_end as usize).saturating_sub(data_begin as usize))
            / size_of::<LlvmProfileData>();

        for i in 0..num_data {
            let data = data_begin.add(i);
            let data_ref = &*data;
            if data_ref.values.is_null() {
                continue;
            }

            let mut total_vsite_count: u32 = 0;
            for vki in IPVK_FIRST..=IPVK_LAST {
                total_vsite_count += data_ref.num_value_sites[(vki - IPVK_FIRST) as usize] as u32;
            }

            let value_counters = data_ref.values as *mut *mut ValueProfNode;

            for site_idx in 0..total_vsite_count {
                let mut curr_vnode = *value_counters.add(site_idx as usize);
                while !curr_vnode.is_null() {
                    (*curr_vnode).count = 0;
                    curr_vnode = (*curr_vnode).next;
                }
            }
        }

        internal::set_profile_dumped(0);
    }
}

/// Marks the profile as dumped.
pub fn set_dumped() {
    internal::set_profile_dumped(1);
}
