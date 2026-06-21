// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Value profiling implementation.
//!
//! Mirrors InstrProfilingValue.c from LLVM compiler-rt.

use core::{cell::UnsafeCell, ffi::c_void, mem::size_of, ptr};

use portable_atomic::{AtomicBool, AtomicPtr, AtomicU32};

use crate::{platform, port, types::*};

static VP_MAX_NUM_VALS_PER_SITE: AtomicU32 = AtomicU32::new(INSTR_PROF_DEFAULT_NUM_VAL_PER_SITE);

static OUT_OF_NODES_WARNINGS: AtomicU32 = AtomicU32::new(0);
const INSTR_PROF_MAX_VP_WARNS: u32 = 10;

/// Atomic bump pointer for allocating value nodes from the static pool.
static CURRENT_VNODE: AtomicPtr<ValueProfNode> = AtomicPtr::new(ptr::null_mut());
static END_VNODE: AtomicPtr<ValueProfNode> = AtomicPtr::new(ptr::null_mut());

/// Tracks whether counters were statically allocated by the compiler.
static HAS_STATIC_COUNTERS: AtomicBool = AtomicBool::new(true);

fn ensure_vnode_pool_initialized() {
    let cur = CURRENT_VNODE.load(core::sync::atomic::Ordering::Acquire);
    if cur.is_null() {
        let begin = platform::begin_vnodes() as *mut ValueProfNode;
        let end = platform::end_vnodes() as *mut ValueProfNode;
        END_VNODE.store(end, core::sync::atomic::Ordering::Release);
        CURRENT_VNODE.store(begin, core::sync::atomic::Ordering::Release);
    }
}

/// Runtime record for value profile serialization.
struct ValueProfRuntimeRecord {
    data: *const LlvmProfileData,
    nodes_kind: [*const ValueProfNode; IPVK_NUM_KINDS],
    site_count_array: [*mut u8; IPVK_NUM_KINDS],
}

impl ValueProfRuntimeRecord {
    const fn new() -> Self {
        Self {
            data: ptr::null(),
            nodes_kind: [ptr::null(); IPVK_NUM_KINDS],
            site_count_array: [ptr::null_mut(); IPVK_NUM_KINDS],
        }
    }
}

struct ValueProfRuntimeRecordStorage(UnsafeCell<ValueProfRuntimeRecord>);

impl ValueProfRuntimeRecordStorage {
    const fn new() -> Self {
        Self(UnsafeCell::new(ValueProfRuntimeRecord::new()))
    }

    fn with<R>(&self, f: impl FnOnce(&ValueProfRuntimeRecord) -> R) -> R {
        // SAFETY: after `vp_init_rt_record` populates the runtime record for a
        // serialization pass, subsequent callbacks only read from it.
        unsafe { f(&*self.0.get()) }
    }

    fn with_mut<R>(&self, f: impl FnOnce(&mut ValueProfRuntimeRecord) -> R) -> R {
        // SAFETY: compiler-rt drives value profile serialization as a
        // single-threaded callback sequence. `vp_init_rt_record` is the only
        // writer and fully refreshes the record before readers consume it.
        unsafe { f(&mut *self.0.get()) }
    }
}

// SAFETY: the compiler-rt value profiling callbacks mutate this record only as
// part of a single-threaded serialization sequence, then treat it as read-only
// for the remainder of that sequence.
unsafe impl Sync for ValueProfRuntimeRecordStorage {}

static RT_RECORD: ValueProfRuntimeRecordStorage = ValueProfRuntimeRecordStorage::new();

static mut VP_DATA_READER: VPDataReaderType = VPDataReaderType {
    init_rt_record: vp_init_rt_record,
    get_value_prof_record_header_size: vp_get_value_prof_record_header_size,
    get_first_value_prof_record: vp_get_first_value_prof_record,
    get_num_value_data_for_site: vp_get_num_value_data_for_site,
    get_value_prof_data_size: vp_get_value_prof_data_size,
    get_value_data: vp_get_value_data,
};

pub fn get_vpdo_data_reader() -> *mut VPDataReaderType {
    // SAFETY: this exposes the runtime's process-global reader table through
    // the compiler-rt compatible mutable-pointer ABI. Callers may legally
    // treat the returned pointer as mutable, so the backing storage must be a
    // mutable static.
    core::ptr::addr_of_mut!(VP_DATA_READER)
}

pub fn set_max_vals_per_site(max_vals: u32) {
    VP_MAX_NUM_VALS_PER_SITE.store(max_vals, core::sync::atomic::Ordering::Relaxed);
}

pub fn setup_value_profiler() {}

/// Allocates the value profile counter array for a function on first use.
/// Mirrors `allocateValueProfileCounters` in InstrProfilingValue.c.
unsafe fn allocate_value_profile_counters(data: *const LlvmProfileData) -> bool {
    // SAFETY: the caller passes a live profile-data record, and this routine
    // initializes its `values` field exactly once via atomic compare-exchange.
    unsafe {
        HAS_STATIC_COUNTERS.store(false, core::sync::atomic::Ordering::Release);

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
    // SAFETY: this routine only allocates nodes from runtime-owned storage and
    // uses atomic bump-pointer updates for the static pool.
    unsafe {
        if !HAS_STATIC_COUNTERS.load(core::sync::atomic::Ordering::Acquire) {
            return port::alloc_zeroed(
                size_of::<ValueProfNode>(),
                core::mem::align_of::<ValueProfNode>(),
            ) as *mut ValueProfNode;
        }

        ensure_vnode_pool_initialized();
        let end_vnode = END_VNODE.load(core::sync::atomic::Ordering::Acquire);

        // Atomic bump allocation.
        loop {
            let current = CURRENT_VNODE.load(core::sync::atomic::Ordering::Acquire);
            if current.is_null() || current >= end_vnode {
                break;
            }
            let next = current.add(1);
            // Due to section padding, EndVNode may point past an incomplete node.
            if next > end_vnode {
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

        let warnings = OUT_OF_NODES_WARNINGS.load(core::sync::atomic::Ordering::Acquire);
        if warnings < INSTR_PROF_MAX_VP_WARNS {
            OUT_OF_NODES_WARNINGS.fetch_add(1, core::sync::atomic::Ordering::AcqRel);
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
    // SAFETY: this wrapper forwards the caller's validated profiling arguments unchanged.
    unsafe { instrument_target_value(target_value, data, counter_index, 1) }
}

/// Records a target value with an explicit count.
/// Mirrors `instrumentTargetValueImpl` in InstrProfilingValue.c.
///
/// # Safety
///
/// `data` must point to a live `LlvmProfileData` record whose value-profiling
/// metadata includes `counter_index`, and the caller must serialize concurrent
/// updates the same way LLVM's profiling runtime expects.
pub unsafe fn instrument_target_value(
    target_value: u64,
    data: *mut c_void,
    counter_index: u32,
    count_value: u64,
) {
    // SAFETY: the caller provides a valid profiling record and site index for
    // this instrumentation update.
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
        } else if !HAS_STATIC_COUNTERS.load(core::sync::atomic::Ordering::Acquire) {
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
    // SAFETY: this wrapper forwards the caller's validated profiling arguments
    // after bucketing the observed memop size.
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
    // SAFETY: compiler-rt calls this with a valid function profile record and
    // writable site-count arrays for the current serialization pass.
    unsafe {
        RT_RECORD.with_mut(|record| {
            record.data = data;
            let nodes = (*data).values as *mut *mut ValueProfNode;
            let mut num_value_kinds: u32 = 0;
            let mut site_offset: usize = 0;

            for vk in IPVK_FIRST..=IPVK_LAST {
                record.nodes_kind[vk as usize] = ptr::null();
                record.site_count_array[vk as usize] = ptr::null_mut();

                let n = (*data).num_value_sites[(vk - IPVK_FIRST) as usize];
                if n == 0 {
                    continue;
                }

                num_value_kinds += 1;

                if !nodes.is_null() {
                    record.nodes_kind[vk as usize] = nodes.add(site_offset) as *const ValueProfNode;
                }
                record.site_count_array[vk as usize] = *site_count_array.add(vk as usize);

                for j in 0..n as usize {
                    let mut c: u32 = 0;
                    let mut site: *const ValueProfNode = ptr::null();
                    if !nodes.is_null() && !record.nodes_kind[vk as usize].is_null() {
                        site = *record.nodes_kind[vk as usize]
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
        })
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
    // SAFETY: `data` points to the start of a `ValueProfData` blob and the
    // first record begins immediately after that fixed-size header.
    unsafe { (data as *mut u8).add(size_of::<ValueProfData>()) as *mut ValueProfRecord }
}

unsafe extern "C" fn vp_get_num_value_data_for_site(value_kind: u32, site: u32) -> u32 {
    // SAFETY: the runtime record is initialized by `vp_init_rt_record` before
    // this callback runs, and the site-count array bounds are driven by LLVM metadata.
    RT_RECORD.with(|record| unsafe {
        if record.site_count_array[value_kind as usize].is_null() {
            return 0;
        }
        *record.site_count_array[value_kind as usize].add(site as usize) as u32
    })
}

unsafe extern "C" fn vp_get_value_prof_data_size() -> u32 {
    // SAFETY: the runtime record is initialized by `vp_init_rt_record` before
    // this callback runs, and all reads stay within that record's metadata.
    RT_RECORD.with(|record| unsafe {
        let data = record.data;
        if data.is_null() {
            return 0;
        }

        let mut total_size: u32 = size_of::<ValueProfData>() as u32;
        let mut num_value_kinds: u32 = 0;

        for vk in IPVK_FIRST..=IPVK_LAST {
            let num_sites = (*data).num_value_sites[(vk - IPVK_FIRST) as usize];
            if num_sites == 0 || record.site_count_array[vk as usize].is_null() {
                continue;
            }

            num_value_kinds += 1;

            // Record header.
            total_size += vp_get_value_prof_record_header_size(num_sites as u32);

            // Value data for each site.
            for site in 0..num_sites as usize {
                let n = *record.site_count_array[vk as usize].add(site) as u32;
                total_size += n * size_of::<InstrProfValueData>() as u32;
            }
        }

        if num_value_kinds == 0 {
            return 0;
        }
        total_size
    })
}

unsafe extern "C" fn vp_get_value_data(
    value_kind: u32,
    site: u32,
    dst: *mut InstrProfValueData,
    start_node: *mut ValueProfNode,
    n: u32,
) -> *mut ValueProfNode {
    // SAFETY: the runtime record is initialized by `vp_init_rt_record`; `dst`
    // points to space for `n` value records, and traversal stays within the
    // per-site linked list owned by the profiling runtime.
    RT_RECORD.with(|record| unsafe {
        let mut vnode = if !start_node.is_null() {
            start_node
        } else {
            let nodes_kind = record.nodes_kind[value_kind as usize];
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
    })
}

#[cfg(unittest)]
mod tests {
    extern crate alloc;

    use alloc::vec::Vec;
    use core::{ffi::c_void, mem::size_of};

    use unittest::{assert, assert_eq, def_test};

    use super::*;

    fn new_profile_data(num_value_sites: [u16; IPVK_NUM_KINDS]) -> LlvmProfileData {
        LlvmProfileData {
            name_ref: 0,
            func_hash: 0,
            counter_ptr: ptr::null_mut(),
            bitmap_ptr: ptr::null_mut(),
            function_pointer: ptr::null_mut(),
            values: ptr::null_mut(),
            num_counters: 0,
            num_value_sites,
            num_bitmap_bytes: 0,
        }
    }

    unsafe fn collect_site_entries(data: &LlvmProfileData, site_index: usize) -> Vec<(u64, u64)> {
        let mut result = Vec::new();
        let counters = data.values.cast::<*mut ValueProfNode>();
        if counters.is_null() {
            return result;
        }
        let mut node = unsafe { *counters.add(site_index) };
        while !node.is_null() {
            result.push((unsafe { (*node).value }, unsafe { (*node).count }));
            node = unsafe { (*node).next };
        }
        result
    }

    unsafe fn free_profile_nodes(data: &mut LlvmProfileData) {
        let values = data.values.cast::<*mut ValueProfNode>();
        if values.is_null() {
            return;
        }

        let mut num_sites = 0usize;
        for vki in IPVK_FIRST..=IPVK_LAST {
            num_sites += data.num_value_sites[(vki - IPVK_FIRST) as usize] as usize;
        }

        for site in 0..num_sites {
            let mut node = unsafe { *values.add(site) };
            while !node.is_null() {
                let next = unsafe { (*node).next };
                unsafe {
                    port::dealloc(
                        node.cast::<u8>(),
                        size_of::<ValueProfNode>(),
                        core::mem::align_of::<ValueProfNode>(),
                    );
                }
                node = next;
            }
        }

        unsafe {
            port::dealloc(
                values.cast::<u8>(),
                num_sites * size_of::<*mut ValueProfNode>(),
                core::mem::align_of::<*mut ValueProfNode>(),
            );
        }
        data.values = ptr::null_mut();
    }

    #[def_test(serial)]
    fn range_rep_value_small() {
        assert_eq!(get_range_rep_value(0), 0);
        assert_eq!(get_range_rep_value(1), 1);
        assert_eq!(get_range_rep_value(8), 8);
    }

    #[def_test(serial)]
    fn range_rep_value_bucketed() {
        // Values from C InstrProfGetRangeRepValue comments/examples.
        assert_eq!(get_range_rep_value(16), 16);
        assert_eq!(get_range_rep_value(9), 9);
        assert_eq!(get_range_rep_value(22), 17);
        assert_eq!(get_range_rep_value(99), 65);
        assert_eq!(get_range_rep_value(256), 256);
        assert_eq!(get_range_rep_value(512), 512);
        assert_eq!(get_range_rep_value(300), 257);
        assert_eq!(get_range_rep_value(513), 513);
        assert_eq!(get_range_rep_value(1000), 513);
    }

    #[def_test(serial)]
    fn instrument_target_value_accumulates_and_links_sites() {
        let old_max = VP_MAX_NUM_VALS_PER_SITE.load(core::sync::atomic::Ordering::Relaxed);
        VP_MAX_NUM_VALS_PER_SITE.store(4, core::sync::atomic::Ordering::Relaxed);

        let mut data = new_profile_data([1, 0, 0]);
        unsafe {
            instrument_target_value(
                7,
                (&mut data as *mut LlvmProfileData).cast::<c_void>(),
                0,
                1,
            );
            instrument_target_value(
                7,
                (&mut data as *mut LlvmProfileData).cast::<c_void>(),
                0,
                3,
            );
            instrument_target_value(
                9,
                (&mut data as *mut LlvmProfileData).cast::<c_void>(),
                0,
                2,
            );

            let entries = collect_site_entries(&data, 0);
            assert_eq!(entries.len(), 2);
            assert_eq!(entries[0], (7, 4));
            assert_eq!(entries[1], (9, 2));

            free_profile_nodes(&mut data);
        }

        VP_MAX_NUM_VALS_PER_SITE.store(old_max, core::sync::atomic::Ordering::Relaxed);
    }

    #[def_test(serial)]
    fn instrument_target_value_evicts_or_decays_min_count_entry() {
        let old_max = VP_MAX_NUM_VALS_PER_SITE.load(core::sync::atomic::Ordering::Relaxed);
        VP_MAX_NUM_VALS_PER_SITE.store(2, core::sync::atomic::Ordering::Relaxed);

        let mut data = new_profile_data([1, 0, 0]);
        unsafe {
            let data_ptr = (&mut data as *mut LlvmProfileData).cast::<c_void>();
            instrument_target_value(1, data_ptr, 0, 5);
            instrument_target_value(2, data_ptr, 0, 2);
            instrument_target_value(3, data_ptr, 0, 3);

            let entries = collect_site_entries(&data, 0);
            assert!(entries.contains(&(1, 5)));
            assert!(entries.contains(&(3, 3)));
            assert!(!entries.iter().any(|&(value, _)| value == 2));

            instrument_target_value(4, data_ptr, 0, 1);
            let entries = collect_site_entries(&data, 0);
            assert!(entries.contains(&(1, 5)));
            assert!(entries.contains(&(3, 2)));
            assert!(!entries.iter().any(|&(value, _)| value == 4));

            free_profile_nodes(&mut data);
        }

        VP_MAX_NUM_VALS_PER_SITE.store(old_max, core::sync::atomic::Ordering::Relaxed);
    }

    #[def_test(serial)]
    fn instrument_memop_buckets_large_values() {
        let old_max = VP_MAX_NUM_VALS_PER_SITE.load(core::sync::atomic::Ordering::Relaxed);
        VP_MAX_NUM_VALS_PER_SITE.store(4, core::sync::atomic::Ordering::Relaxed);

        let mut data = new_profile_data([0, 1, 0]);
        unsafe {
            instrument_memop(300, (&mut data as *mut LlvmProfileData).cast::<c_void>(), 0);
            let entries = collect_site_entries(&data, 0);
            assert_eq!(entries, alloc::vec![(257, 1)]);
            free_profile_nodes(&mut data);
        }

        VP_MAX_NUM_VALS_PER_SITE.store(old_max, core::sync::atomic::Ordering::Relaxed);
    }

    #[def_test(serial)]
    fn vp_data_reader_reports_site_counts_and_values() {
        let old_max = VP_MAX_NUM_VALS_PER_SITE.load(core::sync::atomic::Ordering::Relaxed);
        VP_MAX_NUM_VALS_PER_SITE.store(4, core::sync::atomic::Ordering::Relaxed);

        let mut data = new_profile_data([1, 0, 0]);
        unsafe {
            let data_ptr = (&mut data as *mut LlvmProfileData).cast::<c_void>();
            instrument_target_value(11, data_ptr, 0, 1);
            instrument_target_value(22, data_ptr, 0, 5);

            let mut indirect_counts = [0u8; 8];
            let mut site_count_arrays = [ptr::null_mut(); IPVK_NUM_KINDS];
            site_count_arrays[IPVK_INDIRECT_CALL_TARGET as usize] = indirect_counts.as_mut_ptr();

            let reader = &*get_vpdo_data_reader();
            let num_kinds = (reader.init_rt_record)(&data, site_count_arrays.as_mut_ptr());
            assert_eq!(num_kinds, 1);
            assert_eq!(
                (reader.get_num_value_data_for_site)(IPVK_INDIRECT_CALL_TARGET, 0),
                2
            );
            assert!(indirect_counts[0] >= 2);
            assert_eq!((reader.get_value_prof_data_size)(), 56);

            let mut out = [InstrProfValueData::default(); 2];
            let next = (reader.get_value_data)(
                IPVK_INDIRECT_CALL_TARGET,
                0,
                out.as_mut_ptr(),
                ptr::null_mut(),
                2,
            );
            assert!(next.is_null());
            assert_eq!(out[0].value, 11);
            assert_eq!(out[0].count, 1);
            assert_eq!(out[1].value, 22);
            assert_eq!(out[1].count, 5);

            free_profile_nodes(&mut data);
        }

        VP_MAX_NUM_VALS_PER_SITE.store(old_max, core::sync::atomic::Ordering::Relaxed);
    }
}
