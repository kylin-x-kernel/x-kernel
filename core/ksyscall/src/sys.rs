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
use kprocess::{current_user_process, current_user_process_fs_context};
use kvfs::{Filename, NodePermission};
use linux_raw_sys::{
    ctypes::c_char,
    general::{GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM},
    system::{new_utsname, sysinfo},
};
use osvm::write_vm_mem;
use posix_types::UserPtr;

// Static kernel build constants for uname fields that never change per namespace.
const UTS_SYSNAME: &[u8] = b"Linux";
const UTS_RELEASE: &[u8] = b"10.0.0";
const UTS_VERSION: &[u8] = b"10.0.0";

// Precomputed uname fields whose inputs are compile-time constants. Building
// them as consts eliminates the per-call buffer init/copy on the uname hot
// path (e.g. container init, system-info probing). Only nodename and
// domainname vary per UTS namespace and are filled at runtime below.
const UNAME_SYSNAME: [c_char; 65] = pad_field::<65>(UTS_SYSNAME);
const UNAME_RELEASE: [c_char; 65] = pad_field::<65>(UTS_RELEASE);
const UNAME_VERSION: [c_char; 65] = pad_field::<65>(UTS_VERSION);
const UNAME_MACHINE: [c_char; 65] = pad_field::<65>(ARCH.as_bytes());

/// Pads `src` into a fixed NUL-terminated `c_char` array of length `N`.
///
/// The destination is zero-filled and at most `N - 1` source bytes are copied,
/// leaving the final slot as a NUL terminator. The destination size is fixed
/// at compile time, so the bound is enforced without runtime checks. Marked
/// `const` so callers can precompute fields whose inputs are compile-time
/// constants.
const fn pad_field<const N: usize>(src: &[u8]) -> [c_char; N] {
    let mut buf: [c_char; N] = [0; N];
    let copy_len = if src.len() < N - 1 { src.len() } else { N - 1 };
    // Copy byte by byte to avoid `transmute`, which would be UB on targets
    // where `c_char` is `i8` (it changes signedness and violates strict
    // aliasing). ASCII bytes are representable in both signed and unsigned
    // `c_char`.
    let mut i = 0;
    while i < copy_len {
        buf[i] = src[i] as c_char;
        i += 1;
    }
    buf
}

/// Get system information including OS name, version, and hardware platform
pub fn sys_uname(name: UserPtr<new_utsname>) -> KResult<isize> {
    let uts_ns = current_user_process().uts_ns()?;

    // Read both per-namespace names into stack buffers in a single locked
    // read, avoiding the two heap allocations of nodename()/domainname().
    let mut nodename_buf = [0 as c_char; 65];
    let mut domainname_buf = [0 as c_char; 65];
    uts_ns.read_names_into(&mut nodename_buf, &mut domainname_buf);

    let utsname = new_utsname {
        sysname: UNAME_SYSNAME,
        nodename: nodename_buf,
        release: UNAME_RELEASE,
        version: UNAME_VERSION,
        machine: UNAME_MACHINE,
        domainname: domainname_buf,
    };
    name.write_vm(utsname)?;
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
    kinfo.procs = kprocess::system_view::process_count() as _;
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

/// Get random bytes from the kernel random source.
pub fn sys_getrandom(buf: *mut u8, len: usize, flags: u32) -> KResult<isize> {
    if len == 0 {
        return Ok(0);
    }
    let flags = GetRandomFlags::from_bits_retain(flags);

    debug!("sys_getrandom <= buf: {buf:p}, len: {len}, flags: {flags:?}");

    // TODO: replace this VFS fallback with a real kernel RNG backend.
    // getrandom(2) should not depend on pathname lookup or /dev/random.
    let path = if flags.contains(GetRandomFlags::RANDOM) {
        "/dev/random"
    } else {
        "/dev/urandom"
    };

    let fs_struct = current_user_process_fs_context();
    let fs = fs_struct.lock();
    let file =
        Filename::new(path).open_with_flags_at(fs.root(), fs.pwd(), 0, NodePermission::empty())?;
    drop(fs);
    let mut kbuf = alloc::vec![0; len];
    let mut pos = 0;
    let len = file.read_from(&mut kbuf, &mut pos)?;

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
