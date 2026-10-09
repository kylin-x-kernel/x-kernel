// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! ABI-facing system information structures.

use core::mem::{align_of, offset_of, size_of};

use linux_raw_sys::{
    general::{__kernel_long_t, __kernel_ulong_t},
    system::{new_utsname, sysinfo},
};

use crate::UserWrite;

// SAFETY: `new_utsname` consists only of `c_char` arrays, so it has no padding.
unsafe impl UserWrite for new_utsname {}

/// A `struct sysinfo` carrier used at the syscall boundary.
///
/// `linux_raw_sys::system::sysinfo` has four bytes of implicit padding after
/// `pad` and four trailing bytes on 64-bit targets. Copying that type to user
/// memory would expose uninitialized kernel bytes, so this carrier models both
/// gaps as explicit fields that [`Default`] sets to zero.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, UserWrite)]
pub struct k_sysinfo {
    /// Seconds since boot.
    pub uptime: __kernel_long_t,
    /// 1, 5, and 15 minute load averages.
    pub loads: [__kernel_ulong_t; 3],
    /// Total usable main memory size.
    pub totalram: __kernel_ulong_t,
    /// Available memory size.
    pub freeram: __kernel_ulong_t,
    /// Amount of shared memory.
    pub sharedram: __kernel_ulong_t,
    /// Memory used by buffers.
    pub bufferram: __kernel_ulong_t,
    /// Total swap space size.
    pub totalswap: __kernel_ulong_t,
    /// Swap space still available.
    pub freeswap: __kernel_ulong_t,
    /// Number of current processes.
    pub procs: u16,
    /// Explicit Linux ABI padding field.
    pub pad: u16,
    abi_pad: u32,
    /// Total high memory size.
    pub totalhigh: __kernel_ulong_t,
    /// Available high memory size.
    pub freehigh: __kernel_ulong_t,
    /// Memory unit size in bytes.
    pub mem_unit: u32,
    abi_pad_tail: u32,
}

const _: () = {
    assert!(size_of::<k_sysinfo>() == size_of::<sysinfo>());
    assert!(align_of::<k_sysinfo>() == align_of::<sysinfo>());
    assert!(offset_of!(k_sysinfo, procs) == offset_of!(sysinfo, procs));
    assert!(offset_of!(k_sysinfo, totalhigh) == offset_of!(sysinfo, totalhigh));
    assert!(offset_of!(k_sysinfo, mem_unit) == offset_of!(sysinfo, mem_unit));
};
