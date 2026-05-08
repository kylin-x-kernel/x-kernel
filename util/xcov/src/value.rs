// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Value profiling implementation.
//!
//! Mirrors InstrProfilingValue.c from LLVM compiler-rt.

use core::{ffi::c_void, mem::size_of, ptr};

use portable_atomic::{AtomicPtr, AtomicU32};

use crate::{platform, port, types::*};

static VP_MAX_NUM_VALS_PER_SITE: AtomicU32 = AtomicU32::new(INSTR_PROF_DEFAULT_NUM_VAL_PER_SITE);

static mut OUT_OF_NODES_WARNINGS: u32 = 0;
const INSTR_PROF_MAX_VP_WARNS: u32 = 10;

/// Atomic bump pointer for allocating value nodes from the static pool.
static CURRENT_VNODE: AtomicPtr<ValueProfNode> = AtomicPtr::new(ptr::null_mut());
static mut END_VNODE: *const ValueProfNode = ptr::null();

/// Tracks whether counters were statically allocated by the compiler.
static mut HAS_STATIC_COUNTERS: bool = true;

fn ensure_vnode_pool_initialized() {
    // Safety: single-threaded context (profiling is not concurrent).
    unsafe {
        let cur = CURRENT_VNODE.load(core::sync::atomic::Ordering::Acquire);
        if cur.is_null() {
            let begin = platform::begin_vnodes() as *mut ValueProfNode;
            let end = platform::end_vnodes();
            CURRENT_VNODE.store(begin, core::sync::atomic::Ordering::Release);
            END_VNODE = end;
        }
    }
}

/// Runtime record for value profile serialization.
struct ValueProfRuntimeRecord {
    data: *const LlvmProfileData,
    nodes_kind: [*const ValueProfNode; IPVK_NUM_KINDS],
    site_count_array: [*mut u8; IPVK_NUM_KINDS],
}

static mut RT_RECORD: ValueProfRuntimeRecord = ValueProfRuntimeRecord {
    data: ptr::null(),
    nodes_kind: [ptr::null(); IPVK_NUM_KINDS],
    site_count_array: [ptr::null_mut(); IPVK_NUM_KINDS],
};

static mut VP_DATA_READER: VPDataReaderType = VPDataReaderType {
    init_rt_record: vp_init_rt_record,
    get_value_prof_record_header_size: vp_get_value_prof_record_header_size,
    get_first_value_prof_record: vp_get_first_value_prof_record,
    get_num_value_data_for_site: vp_get_num_value_data_for_site,
    get_value_prof_data_size: vp_get_value_prof_data_size,
    get_value_data: vp_get_value_data,
};

pub fn get_vpdo_data_reader() -> *mut VPDataReaderType {
    &raw mut VP_DATA_READER
}

pub fn set_max_vals_per_site(max_vals: u32) {
    VP_MAX_NUM_VALS_PER_SITE.store(max_vals, core::sync::atomic::Ordering::Relaxed);
}

pub fn setup_value_profiler() {}

/// Allocates the value profile counter array for a function on first use.
/// Mirrors `allocateValueProfileCounters` in InstrProfilingValue.c.
unsafe fn allocate_value_profile_counters(data: *const LlvmProfileData) -> bool {
    unsafe {
        HAS_STATIC_COUNTERS = false;

        let mut num_vsites: u32 = 0;
        for vki in IPVK_FIRST..=IPVK_LAST {
            num_vsites += (*data).num_value_sites[(vki - IPVK_FIRST) as usize] as u32;
        }

        if num_vsites == 0 {
            return false;
        }

        let size = num_vsites as usize * size_of::<*mut ValueProfNode>();
        let align = core::mem::align_of::<*mut ValueProfNode>();
        let mem = port::alloc_zeroed(size, align) as *mut *mut ValueProfNode;
        if mem.is_null() {
            return false;
        }

        let data_mut = data as *mut LlvmProfileData;
        let old = port::bool_cmpxchg_u64(&raw mut (*data_mut).values as *mut u64, 0, mem as u64);
        if !old {
            port::dealloc(mem as *mut u8, size, align);
            return false;
        }
        true
    }
}

/// Allocates a single value node from the static pool or dynamically.
/// Mirrors `allocateOneNode` in InstrProfilingValue.c.
unsafe fn allocate_one_node() -> *mut ValueProfNode {
    unsafe {
        if !HAS_STATIC_COUNTERS {
            return port::alloc_zeroed(
                size_of::<ValueProfNode>(),
                core::mem::align_of::<ValueProfNode>(),
            ) as *mut ValueProfNode;
        }

        ensure_vnode_pool_initialized();

        // Atomic bump allocation.
        loop {
            let current = CURRENT_VNODE.load(core::sync::atomic::Ordering::Acquire);
            if current.is_null() || current >= END_VNODE as *mut ValueProfNode {
                break;
            }
            let next = current.add(1);
            // Due to section padding, EndVNode may point past an incomplete node.
            if next > END_VNODE as *mut ValueProfNode {
                break;
            }
            if CURRENT_VNODE
                .compare_exchange(
                    current,
                    next,
                    core::sync::atomic::Ordering::AcqRel,
                    core::sync::atomic::Ordering::Acquire,
                )
                .is_ok()
            {
                return current;
            }
        }

        if OUT_OF_NODES_WARNINGS < INSTR_PROF_MAX_VP_WARNS {
            OUT_OF_NODES_WARNINGS += 1;
        }
        ptr::null_mut()
    }
}

/// Records a target value for indirect call profiling.
///
/// # Safety
///
/// `data` must point to a valid `LlvmProfileData` with value profiling enabled.
pub unsafe fn instrument_target(target_value: u64, data: *mut c_void, counter_index: u32) {
    unsafe { instrument_target_value(target_value, data, counter_index, 1) }
}

/// Records a target value with an explicit count.
/// Mirrors `instrumentTargetValueImpl` in InstrProfilingValue.c.
///
/// # Safety
///
/// Same as `instrument_target`.
pub unsafe fn instrument_target_value(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
    count_value: u64,
) {
    unsafe {
        if data.is_null() || count_value == 0 {
            return;
        }

        let prof_data = data as *mut LlvmProfileData;

        if (*prof_data).values.is_null() && !allocate_value_profile_counters(prof_data) {
            return;
        }

        let value_counters = (*prof_data).values as *mut *mut ValueProfNode;
        let mut prev_vnode: *mut ValueProfNode = ptr::null_mut();
        let mut min_count_vnode: *mut ValueProfNode = ptr::null_mut();
        let mut cur_vnode = *value_counters.add(counter_index as usize);
        let mut min_count = u64::MAX;
        let mut vdata_count: u32 = 0;

        while !cur_vnode.is_null() {
            if (*cur_vnode).value == target_value {
                (*cur_vnode).count += count_value;
                return;
            }
            if (*cur_vnode).count < min_count {
                min_count = (*cur_vnode).count;
                min_count_vnode = cur_vnode;
            }
            prev_vnode = cur_vnode;
            cur_vnode = (*cur_vnode).next;
            vdata_count += 1;
        }

        let max_vals = VP_MAX_NUM_VALS_PER_SITE.load(core::sync::atomic::Ordering::Relaxed);

        if vdata_count >= max_vals {
            // Min-count eviction policy from C code.
            if !min_count_vnode.is_null() && (*min_count_vnode).count <= count_value {
                (*min_count_vnode).value = target_value;
                (*min_count_vnode).count = count_value;
            } else if !min_count_vnode.is_null() {
                (*min_count_vnode).count -= count_value;
            }
            return;
        }

        let new_node = allocate_one_node();
        if new_node.is_null() {
            return;
        }
        (*new_node).value = target_value;
        (*new_node).count += count_value;

        let site_ptr = value_counters.add(counter_index as usize);
        if (*site_ptr).is_null() {
            port::bool_cmpxchg_u64(site_ptr as *mut u64, 0, new_node as u64);
        } else if !prev_vnode.is_null() && (*prev_vnode).next.is_null() {
            port::bool_cmpxchg_u64(&raw mut (*prev_vnode).next as *mut u64, 0, new_node as u64);
        } else if !HAS_STATIC_COUNTERS {
            port::dealloc(
                new_node as *mut u8,
                size_of::<ValueProfNode>(),
                core::mem::align_of::<ValueProfNode>(),
            );
        }
    }
}

/// Records a memory operation size value with log2 bucketing.
///
/// # Safety
///
/// Same as `instrument_target`.
pub unsafe fn instrument_memop(target_value: u64, data: *mut c_void, counter_index: u32) {
    unsafe {
        let rep_value = get_range_rep_value(target_value);
        instrument_target(rep_value, data, counter_index);
    }
}

/// Maps an observed memop size value to the representative value of its range.
// Mirrors InstrProfGetRangeRepValue in InstrProfData.inc.
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

// === VPDataReader callbacks ===

unsafe extern "C" fn vp_init_rt_record(
    data: *const LlvmProfileData,
    site_count_array: *mut *mut u8,
) -> u32 {
    unsafe {
        RT_RECORD.data = data;
        let nodes = (*data).values as *mut *mut ValueProfNode;
        let mut num_value_kinds: u32 = 0;
        let mut site_offset: usize = 0;

        for vk in IPVK_FIRST..=IPVK_LAST {
            RT_RECORD.nodes_kind[vk as usize] = ptr::null();
            RT_RECORD.site_count_array[vk as usize] = ptr::null_mut();

            let n = (*data).num_value_sites[(vk - IPVK_FIRST) as usize];
            if n == 0 {
                continue;
            }

            num_value_kinds += 1;

            if !nodes.is_null() {
                RT_RECORD.nodes_kind[vk as usize] = nodes.add(site_offset) as *const ValueProfNode;
            }

            for j in 0..n as usize {
                let mut c: u32 = 0;
                let mut site: *const ValueProfNode = ptr::null();
                if !nodes.is_null() && !RT_RECORD.nodes_kind[vk as usize].is_null() {
                    site = *RT_RECORD.nodes_kind[vk as usize]
                        .cast::<*const ValueProfNode>()
                        .add(j);
                }
                while !site.is_null() {
                    c += 1;
                    site = (*site).next;
                }
                if c > u8::MAX as u32 {
                    c = u8::MAX as u32;
                }
                let sc_ptr = *site_count_array.add(vk as usize);
                if !sc_ptr.is_null() {
                    *sc_ptr.add(j) = c as u8;
                }
            }
            site_offset += n as usize;
        }
        num_value_kinds
    }
}

unsafe extern "C" fn vp_get_value_prof_record_header_size(num_sites: u32) -> u32 {
    let site_count_array_size = num_sites;
    let header_fixed = size_of::<ValueProfRecord>() as u32;
    let total = header_fixed + site_count_array_size;
    let padding = (7 & (8 - total % 8)) as u32;
    total + padding
}

unsafe extern "C" fn vp_get_first_value_prof_record(
    data: *mut ValueProfData,
) -> *mut ValueProfRecord {
    unsafe { (data as *mut u8).add(size_of::<ValueProfData>()) as *mut ValueProfRecord }
}

unsafe extern "C" fn vp_get_num_value_data_for_site(value_kind: u32, site: u32) -> u32 {
    unsafe {
        if RT_RECORD.site_count_array[value_kind as usize].is_null() {
            return 0;
        }
        *RT_RECORD.site_count_array[value_kind as usize].add(site as usize) as u32
    }
}

unsafe extern "C" fn vp_get_value_prof_data_size() -> u32 {
    unsafe {
        let data = RT_RECORD.data;
        if data.is_null() {
            return 0;
        }

        let mut total_size: u32 = size_of::<ValueProfData>() as u32;
        let mut num_value_kinds: u32 = 0;

        for vk in IPVK_FIRST..=IPVK_LAST {
            let num_sites = (*data).num_value_sites[(vk - IPVK_FIRST) as usize];
            if num_sites == 0 || RT_RECORD.site_count_array[vk as usize].is_null() {
                continue;
            }

            num_value_kinds += 1;

            // Record header.
            total_size += vp_get_value_prof_record_header_size(num_sites as u32);

            // Value data for each site.
            for site in 0..num_sites as usize {
                let n = *RT_RECORD.site_count_array[vk as usize].add(site) as u32;
                total_size += n * size_of::<InstrProfValueData>() as u32;
            }
        }

        if num_value_kinds == 0 {
            return 0;
        }
        total_size
    }
}

unsafe extern "C" fn vp_get_value_data(
    value_kind: u32,
    site: u32,
    dst: *mut InstrProfValueData,
    start_node: *mut ValueProfNode,
    n: u32,
) -> *mut ValueProfNode {
    unsafe {
        let mut vnode = if !start_node.is_null() {
            start_node
        } else {
            let nodes_kind = RT_RECORD.nodes_kind[value_kind as usize];
            if nodes_kind.is_null() {
                return ptr::null_mut();
            }
            *nodes_kind.cast::<*const ValueProfNode>().add(site as usize) as *mut ValueProfNode
        };

        for i in 0..n as usize {
            if vnode.is_null() {
                break;
            }
            *dst.add(i) = InstrProfValueData {
                value: (*vnode).value,
                count: (*vnode).count,
            };
            vnode = (*vnode).next;
        }
        vnode
    }
}

#[cfg(test)]
pub mod tests {
    pub use super::get_range_rep_value;

    #[test]
    fn range_rep_value_small() {
        assert_eq!(get_range_rep_value(0), 0);
        assert_eq!(get_range_rep_value(1), 1);
        assert_eq!(get_range_rep_value(8), 8);
    }

    #[test]
    fn range_rep_value_bucketed() {
        // Values from C InstrProfGetRangeRepValue comments/examples.
        assert_eq!(get_range_rep_value(16), 16); // power of 2 → as-is
        assert_eq!(get_range_rep_value(9), 9); // prev_pow2(9)+1 = 9
        assert_eq!(get_range_rep_value(22), 17); // prev_pow2(22)+1 = 17
        assert_eq!(get_range_rep_value(99), 65); // prev_pow2(99)+1 = 65
        assert_eq!(get_range_rep_value(256), 256); // power of 2 → as-is
        assert_eq!(get_range_rep_value(512), 512); // power of 2 → as-is
        assert_eq!(get_range_rep_value(300), 257); // prev_pow2(300)+1 = 257
        assert_eq!(get_range_rep_value(513), 513); // >= 513 → 513
        assert_eq!(get_range_rep_value(1000), 513);
    }
}
