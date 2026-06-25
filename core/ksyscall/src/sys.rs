// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System information and control syscalls.
//!
//! This module provides syscalls for querying and manipulating system information including:
//! - System information (uname, sysinfo, etc.)
//! - Process information queries
//! - Hostname management

use kbuild_config::ARCH;
use kerrno::KResult;
use kthread::{current_process_fs_context, processes};
use kvfs::{LookupFlags, LookupIntent, lookup_location};
use linux_raw_sys::{
    ctypes::c_char,
    general::{GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM},
    system::{new_utsname, sysinfo},
};
use osvm::write_vm_mem;
use posix_types::UserPtr;

const fn pad_str(info: &str) -> [c_char; 65] {
    let mut data: [c_char; 65] = [0; 65];
    let bytes = info.as_bytes();
    let mut idx = 0;
    while idx < bytes.len() {
        data[idx] = bytes[idx] as c_char;
        idx += 1;
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
pub fn sys_uname(name: UserPtr<new_utsname>) -> KResult<isize> {
    name.write_vm(UTSNAME)?;
    Ok(0)
}

/// Get general system information such as process count and memory unit
pub fn sys_sysinfo(info: UserPtr<sysinfo>) -> KResult<isize> {
    let mut kinfo = sysinfo {
        uptime: 0,
        loads: [0; 3],
        totalram: 0,
        freeram: 0,
        sharedram: 0,
        bufferram: 0,
        totalswap: 0,
        freeswap: 0,
        procs: 0,
        pad: 0,
        totalhigh: 0,
        freehigh: 0,
        mem_unit: 0,
        _f: linux_raw_sys::system::__IncompleteArrayField::new(),
    };
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

    let fs_context = current_process_fs_context();
    let fs = fs_context.lock();
    let f = lookup_location(
        &fs.lookup_context(),
        path,
        LookupIntent::Open,
        LookupFlags::follow(),
    )?;
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
