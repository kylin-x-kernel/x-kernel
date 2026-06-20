// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Profile data serialization.
//!
//! Writes raw profile data in the LLVM .profraw binary format.

use core::{ffi::c_void, mem::size_of, ptr};

use crate::{buffer, internal, platform, port, profiling, types::*};

/// Buffer writer callback — writes to a flat memory buffer.
///
/// # Safety
///
/// `this.writer_ctx` must point to a valid mutable byte buffer pointer.
pub unsafe extern "C" fn buffer_writer(
    this: *mut ProfDataWriter,
    io_vecs: *mut ProfDataIOVec,
    num_io_vecs: u32,
) -> u32 {
    // SAFETY: `this` and `io_vecs` come from the profiling runtime, and the
    // callback only advances the destination pointer within the caller-owned buffer.
    unsafe {
        let mut buffer_ptr = (*this).writer_ctx as *mut u8;

        for i in 0..num_io_vecs as usize {
            let iov = io_vecs.add(i);
            let elm_size = (*iov).elm_size;
            let num_elm = (*iov).num_elm;
            let total_size = elm_size * num_elm;

            if (*iov).data.is_null() {
                if (*iov).use_zero_padding != 0 {
                    port::mem_zero(buffer_ptr, total_size);
                }
                buffer_ptr = buffer_ptr.add(total_size);
                continue;
            }

            port::mem_copy(buffer_ptr, (*iov).data as *const u8, total_size);
            buffer_ptr = buffer_ptr.add(total_size);
        }

        (*this).writer_ctx = buffer_ptr as *mut c_void;
        0
    }
}

/// Main entry point: writes all profile data via the given writer.
///
/// # Safety
///
/// `writer` must point to a valid mutable `ProfDataWriter`,
/// `vp_data_reader` must be a valid value-profiling reader callback block when
/// non-null, and all profile section accessors used by this routine must refer
/// to live profiling sections for the current binary.
pub unsafe fn write_data(
    writer: *mut ProfDataWriter,
    vp_data_reader: *mut VPDataReaderType,
    skip_name_data_write: i32,
) -> i32 {
    // SAFETY: the caller provides a valid writer and section accessors; this
    // wrapper forwards the current runtime-owned section boundaries unchanged.
    unsafe {
        write_data_impl(
            writer,
            platform::begin_data(),
            platform::end_data(),
            platform::begin_counters(),
            platform::end_counters(),
            platform::begin_bitmap(),
            platform::end_bitmap(),
            vp_data_reader,
            platform::begin_names(),
            platform::end_names(),
            skip_name_data_write,
        )
    }
}

/// Writes profile data given explicit section boundaries.
///
/// # Safety
///
/// All pointer arguments must be valid for their respective section ranges.
#[allow(clippy::too_many_arguments)]
pub unsafe fn write_data_impl(
    writer: *mut ProfDataWriter,
    data_begin: *const LlvmProfileData,
    data_end: *const LlvmProfileData,
    counters_begin: *const u8,
    counters_end: *const u8,
    bitmap_begin: *const u8,
    bitmap_end: *const u8,
    vp_data_reader: *mut VPDataReaderType,
    names_begin: *const u8,
    names_end: *const u8,
    skip_name_data_write: i32,
) -> i32 {
    // SAFETY: all section boundary pointers come from the profiling runtime and
    // are treated as read-only ranges during serialization.
    unsafe {
        let num_data = buffer::get_num_data(data_begin, data_end);
        let data_size = buffer::get_data_size(data_begin, data_end);
        let counters_size = buffer::get_counters_size(counters_begin, counters_end);
        let num_bitmap_bytes = buffer::get_num_bitmap_bytes(bitmap_begin, bitmap_end);
        let names_size = buffer::get_name_size(names_begin, names_end);

        let padding = match buffer::get_padding_sizes_for_counters(
            data_size,
            counters_size,
            num_bitmap_bytes,
            names_size,
            0,
            0,
        ) {
            Ok(p) => p,
            Err(_) => return -1,
        };

        let counters_delta = if num_data > 0 {
            (counters_begin as isize - data_begin as isize) as u64
        } else {
            0
        };
        let bitmap_delta = if num_data > 0 {
            (bitmap_begin as isize - data_begin as isize) as u64
        } else {
            0
        };
        let names_delta = if num_data > 0 {
            names_begin as isize as u64
        } else {
            0
        };

        let header = LlvmProfileHeader {
            magic: profiling::get_magic(),
            version: profiling::get_version(),
            binary_ids_size: platform::get_binary_ids_size(),
            num_data,
            padding_bytes_before_counters: padding.before_counters,
            num_counters: buffer::get_num_counters(counters_begin, counters_end),
            padding_bytes_after_counters: padding.after_counters,
            num_bitmap_bytes,
            padding_bytes_after_bitmap_bytes: padding.after_bitmap,
            names_size,
            counters_delta,
            bitmap_delta,
            names_delta,
            num_vtables: 0,
            vnames_size: 0,
            value_kind_last: IPVK_LAST as u64,
        };

        let names_write_len = if skip_name_data_write != 0 {
            0
        } else {
            names_size as usize
        };
        let names_use_zero_pad = if skip_name_data_write != 0 { 1 } else { 0 };

        let mut iovs = [
            ProfDataIOVec {
                data: &header as *const LlvmProfileHeader as *mut c_void,
                elm_size: 1,
                num_elm: size_of::<LlvmProfileHeader>(),
                use_zero_padding: 0,
            },
            ProfDataIOVec {
                data: ptr::null_mut(),
                elm_size: 1,
                num_elm: 0,
                use_zero_padding: 0,
            },
            ProfDataIOVec {
                data: data_begin as *mut c_void,
                elm_size: 1,
                num_elm: data_size as usize,
                use_zero_padding: 0,
            },
            ProfDataIOVec {
                data: ptr::null_mut(),
                elm_size: 1,
                num_elm: padding.before_counters as usize,
                use_zero_padding: 1,
            },
            ProfDataIOVec {
                data: counters_begin as *mut c_void,
                elm_size: 1,
                num_elm: counters_size as usize,
                use_zero_padding: 0,
            },
            ProfDataIOVec {
                data: ptr::null_mut(),
                elm_size: 1,
                num_elm: padding.after_counters as usize,
                use_zero_padding: 1,
            },
            ProfDataIOVec {
                data: bitmap_begin as *mut c_void,
                elm_size: 1,
                num_elm: num_bitmap_bytes as usize,
                use_zero_padding: 0,
            },
            ProfDataIOVec {
                data: ptr::null_mut(),
                elm_size: 1,
                num_elm: padding.after_bitmap as usize,
                use_zero_padding: 1,
            },
            ProfDataIOVec {
                data: names_begin as *mut c_void,
                elm_size: 1,
                num_elm: names_write_len,
                use_zero_padding: names_use_zero_pad,
            },
            ProfDataIOVec {
                data: ptr::null_mut(),
                elm_size: 1,
                num_elm: padding.after_names as usize,
                use_zero_padding: 1,
            },
        ];

        let result = ((*writer).write_fn)(writer, iovs.as_mut_ptr(), iovs.len() as u32);
        if result != 0 {
            return -1;
        }

        // Write value profiling data for each function.
        // Mirrors writeValueProfData in InstrProfilingWriter.c.
        if !(vp_data_reader.is_null() || num_data == 0 && names_size == 0) {
            let reader = &*vp_data_reader;
            for i in 0..num_data as usize {
                let data = data_begin.add(i);
                if write_one_value_prof_data(writer, reader, data) != 0 {
                    return -1;
                }
            }
        }

        internal::set_profile_dumped(1);
        0
    }
}

const VP_DATA_ARRAY_SIZE: usize = 16;
const MAX_SITE_COUNT_BUF: usize = 256 + 8;

/// Writes value profile data for one function.
/// Mirrors `writeOneValueProfData` in InstrProfilingWriter.c.
unsafe fn write_one_value_prof_data(
    writer: *mut ProfDataWriter,
    reader: &VPDataReaderType,
    data: *const LlvmProfileData,
) -> i32 {
    // SAFETY: the writer and value-profile reader come from the runtime, and
    // this routine serializes one function's profiling metadata in place.
    unsafe {
        let mut site_count_bufs: [[u8; MAX_SITE_COUNT_BUF]; IPVK_NUM_KINDS] =
            [[0u8; MAX_SITE_COUNT_BUF]; IPVK_NUM_KINDS];
        let mut site_count_arrays: [*mut u8; IPVK_NUM_KINDS] = [ptr::null_mut(); IPVK_NUM_KINDS];

        for vk in IPVK_FIRST..=IPVK_LAST {
            let n = (*data).num_value_sites[(vk - IPVK_FIRST) as usize];
            if n == 0 {
                continue;
            }
            let sz = (reader.get_value_prof_record_header_size)(n as u32) as usize
                - size_of::<ValueProfRecord>();
            if sz > MAX_SITE_COUNT_BUF {
                return -1;
            }
            site_count_arrays[vk as usize] = site_count_bufs[vk as usize].as_mut_ptr();
            ptr::write_bytes(site_count_arrays[vk as usize], 0, sz);
        }

        let num_value_kinds = (reader.init_rt_record)(data, site_count_arrays.as_mut_ptr());
        if num_value_kinds == 0 {
            return 0;
        }

        let total_size = (reader.get_value_prof_data_size)();
        let vp_header = ValueProfData {
            total_size,
            num_value_kinds,
        };
        let mut iov = ProfDataIOVec {
            data: &vp_header as *const ValueProfData as *mut c_void,
            elm_size: 1,
            num_elm: size_of::<ValueProfData>(),
            use_zero_padding: 0,
        };
        if ((*writer).write_fn)(writer, &mut iov, 1) != 0 {
            return -1;
        }

        for vk in IPVK_FIRST..=IPVK_LAST {
            let n = (*data).num_value_sites[(vk - IPVK_FIRST) as usize];
            if n == 0 {
                continue;
            }

            let record_header = ValueProfRecord {
                kind: vk,
                num_value_sites: n as u32,
            };
            let record_header_size = size_of::<ValueProfRecord>();
            let mut iov = ProfDataIOVec {
                data: &record_header as *const ValueProfRecord as *mut c_void,
                elm_size: 1,
                num_elm: record_header_size,
                use_zero_padding: 0,
            };
            if ((*writer).write_fn)(writer, &mut iov, 1) != 0 {
                return -1;
            }

            let site_count_array_size =
                (reader.get_value_prof_record_header_size)(n as u32) as usize - record_header_size;
            let mut iov = ProfDataIOVec {
                data: site_count_arrays[vk as usize] as *mut c_void,
                elm_size: 1,
                num_elm: site_count_array_size,
                use_zero_padding: 0,
            };
            if ((*writer).write_fn)(writer, &mut iov, 1) != 0 {
                return -1;
            }

            let mut vp_data_array: [InstrProfValueData; VP_DATA_ARRAY_SIZE] =
                [InstrProfValueData::default(); VP_DATA_ARRAY_SIZE];

            for site in 0..n as usize {
                let mut n_remain = (reader.get_num_value_data_for_site)(vk, site as u32);
                if n_remain == 0 {
                    continue;
                }
                let mut next_start_node: *mut ValueProfNode = ptr::null_mut();
                while n_remain > 0 {
                    let n_read = if n_remain > VP_DATA_ARRAY_SIZE as u32 {
                        VP_DATA_ARRAY_SIZE as u32
                    } else {
                        n_remain
                    };
                    next_start_node = (reader.get_value_data)(
                        vk,
                        site as u32,
                        vp_data_array.as_mut_ptr(),
                        next_start_node,
                        n_read,
                    );
                    let mut iov = ProfDataIOVec {
                        data: vp_data_array.as_ptr() as *mut c_void,
                        elm_size: size_of::<InstrProfValueData>(),
                        num_elm: n_read as usize,
                        use_zero_padding: 0,
                    };
                    if ((*writer).write_fn)(writer, &mut iov, 1) != 0 {
                        return -1;
                    }
                    n_remain -= n_read;
                }
            }
        }

        0
    }
}

/// Writes the profile data to the given buffer.
///
/// # Safety
///
/// `out_buffer` must point to a buffer at least as large as
/// `buffer::get_size_for_buffer()` bytes.
pub unsafe fn write_buffer(out_buffer: *mut u8) -> i32 {
    // SAFETY: the caller provides an output buffer large enough for the full
    // serialized profile, and the temporary writer stays within that range.
    unsafe {
        let mut writer = ProfDataWriter {
            write_fn: buffer_writer,
            writer_ctx: out_buffer as *mut c_void,
        };
        write_data(&mut writer, ptr::null_mut(), 0)
    }
}
