// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System information and control syscalls.
//!
//! This module provides syscalls for querying and manipulating system information including:
//! - System information (uname, sysinfo, etc.)
//! - Process information queries
//! - Hostname management

use core::ffi::c_char;

use kbuild_config::ARCH;
use kerrno::KResult;
use kthread::{current_process_fs_context, processes};
use linux_raw_sys::{
    general::{GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM},
    system::{new_utsname, sysinfo},
};
use osvm::{VirtMutPtr, write_vm_mem};

const fn pad_str(info: &str) -> [c_char; 65] {
    let mut data: [c_char; 65] = [0; 65];
    // this needs #![feature(const_copy_from_slice)]
    // data[..info.len()].copy_from_slice(info.as_bytes());
    unsafe {
        core::ptr::copy_nonoverlapping(info.as_ptr().cast(), data.as_mut_ptr(), info.len());
    }
    data
}

// Compatible with Linux
const UTSNAME: new_utsname = new_utsname {
    sysname: pad_str("Linux"),
    nodename: pad_str("kylin-x"),
    release: pad_str("10.0.0"),
    version: pad_str("10.0.0"),
    machine: pad_str(ARCH),
    domainname: pad_str("https://gitee/openkylin/x-kernel"),
};

/// Get system information including OS name, version, and hardware platform
pub fn sys_uname(name: *mut new_utsname) -> KResult<isize> {
    name.write_vm(UTSNAME)?;
    Ok(0)
}

/// Get general system information such as process count and memory unit
pub fn sys_sysinfo(info: *mut sysinfo) -> KResult<isize> {
    // FIXME: Zeroable
    let mut kinfo: sysinfo = unsafe { core::mem::zeroed() };
    kinfo.procs = processes().len() as _;
    kinfo.mem_unit = 1;
    info.write_vm(kinfo)?;
    Ok(0)
}

/// Access kernel log buffer (syslog)
pub fn sys_syslog(_type: i32, _buf: *mut c_char, _len: usize) -> KResult<isize> {
    Ok(0)
}

bitflags::bitflags! {
    #[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
    pub struct GetRandomFlags: u32 {
        const NONBLOCK = GRND_NONBLOCK;
        const RANDOM = GRND_RANDOM;
        const INSECURE = GRND_INSECURE;
    }
}

/// Get random bytes from /dev/urandom or /dev/random
pub fn sys_getrandom(buf: *mut u8, len: usize, flags: u32) -> KResult<isize> {
    if len == 0 {
        return Ok(0);
    }
    let flags = GetRandomFlags::from_bits_retain(flags);

    debug!("sys_getrandom <= buf: {buf:p}, len: {len}, flags: {flags:?}");

    let path = if flags.contains(GetRandomFlags::RANDOM) {
        "/dev/random"
    } else {
        "/dev/urandom"
    };

    let f = current_process_fs_context().lock().resolve(path)?;
    let mut kbuf = alloc::vec![0; len];
    let len = f.entry().as_file()?.read_at(&mut kbuf, 0)?;

    write_vm_mem(buf, &kbuf)?;

    Ok(len as _)
}

/// Secure computing syscall for sandboxing (not fully implemented)
pub fn sys_seccomp(_op: u32, _flags: u32, _args: *const ()) -> KResult<isize> {
    warn!("dummy sys_seccomp");
    Ok(0)
}

/// Flush instruction cache (RISC-V architecture only)
#[cfg(target_arch = "riscv64")]
pub fn sys_riscv_flush_icache() -> KResult<isize> {
    riscv::asm::fence_i();
    Ok(0)
}
