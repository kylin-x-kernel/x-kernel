// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! System information and control syscalls.
//!
//! This module provides syscalls for querying and manipulating system information including:
//! - System information (uname, sysinfo, etc.)
//! - Process information queries
//! - Hostname management
//! - Power and reboot control

use core::{mem::MaybeUninit, slice};

use kbuild_config::ARCH;
use kerrno::{KError, KResult};
use khal::mem;
use kprocess::{current_user_process, current_user_process_fs_context};
use kvfs::{Filename, NodePermission};
use linux_raw_sys::{
    ctypes::c_char,
    general::{
        GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM, LINUX_REBOOT_CMD_CAD_OFF,
        LINUX_REBOOT_CMD_CAD_ON, LINUX_REBOOT_CMD_HALT, LINUX_REBOOT_CMD_POWER_OFF,
        LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2, LINUX_REBOOT_MAGIC2A, LINUX_REBOOT_MAGIC2B,
        LINUX_REBOOT_MAGIC2C,
    },
    system::{new_utsname, sysinfo},
};
use osvm::{VirtPtr, write_vm_mem};
use posix_types::{UserConstPtr, UserPtr};

/// Maximum hostname length in bytes accepted by `sethostname(2)`.
///
/// Matches `UTS_LEN - 1` enforced by the UTS namespace owner
/// (`process/kns/src/uts.rs`); kept as a local constant rather than exposing
/// the owner's internal limit across crates.
const MAX_HOSTNAME_LEN: usize = 64;

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

    let alloc = kalloc::global_allocator();
    let avail = alloc.available_pages();
    let total_pages = alloc.used_pages() + avail;
    kinfo.totalram = total_pages.saturating_mul(mem::PAGE_SIZE_4K) as _;
    kinfo.freeram = avail.saturating_mul(mem::PAGE_SIZE_4K) as _;

    info.write_vm(kinfo)?;
    Ok(0)
}

/// Access kernel log buffer (syslog)
pub fn sys_syslog(_type: i32, _buf: *mut c_char, _len: usize) -> KResult<isize> {
    Ok(0)
}

/// Sets the hostname in the calling process's UTS namespace.
pub fn sys_sethostname(name: UserConstPtr<u8>, len: usize) -> KResult<isize> {
    if !kprocess::current_cred().is_privileged() {
        return Err(KError::OperationNotPermitted);
    }
    if len > MAX_HOSTNAME_LEN {
        return Err(KError::InvalidInput);
    }

    // Hostnames are short (<= 64 B); read into a stack buffer instead of
    // allocating a `Vec`, mirroring `sys_uname`'s stack-buffer style.
    let mut buf = [0u8; MAX_HOSTNAME_LEN];
    // SAFETY: `buf` is a live `[u8; N]` of trivially-initializable bytes, so its
    // first `len` slots may be reborrowed as `MaybeUninit<u8>` for copy-from-user
    // (same pattern as `devfs/nodes/loop.rs`). Only `buf[..len]` is read below.
    let uninit =
        unsafe { slice::from_raw_parts_mut(buf.as_mut_ptr().cast::<MaybeUninit<u8>>(), len) };
    osvm::read_vm_bytes(name.as_ptr(), uninit)?;
    kprocess::current_user_process()
        .uts_ns()?
        .set_nodename(&buf[..len])
        .map_err(|_| KError::InvalidInput)?;
    Ok(0)
}

/// Applies a Linux reboot control command supported by the current platform.
///
/// Requires a privileged credential and a valid reboot magic pair
/// (`LINUX_REBOOT_MAGIC1` plus one of the `MAGIC2*` values), matching the
/// `reboot(2)` ABI. Only `HALT`, `POWER_OFF` and the `CAD_ON`/`CAD_OFF`
/// toggles are handled here; other commands (`RESTART`, `RESTART2`, `KEXEC`,
/// `SW_SUSPEND`) are rejected with `EINVAL`, since `reboot(2)` returns
/// `EINVAL` — not `ENOSYS` — for an unsupported command.
pub fn sys_reboot(
    magic1: u32,
    magic2: u32,
    command: u32,
    _argument: UserConstPtr<c_char>,
) -> KResult<isize> {
    if !kprocess::current_cred().is_privileged() {
        return Err(KError::OperationNotPermitted);
    }
    if magic1 != LINUX_REBOOT_MAGIC1
        || !matches!(
            magic2,
            LINUX_REBOOT_MAGIC2
                | LINUX_REBOOT_MAGIC2A
                | LINUX_REBOOT_MAGIC2B
                | LINUX_REBOOT_MAGIC2C
        )
    {
        return Err(KError::InvalidInput);
    }

    match command {
        // CAD_ON/CAD_OFF toggle the Ctrl-Alt-Del behaviour. There is no kernel
        // CAD-state variable yet, so these are accepted as an intentional stub.
        LINUX_REBOOT_CMD_CAD_ON | LINUX_REBOOT_CMD_CAD_OFF => Ok(0),
        LINUX_REBOOT_CMD_HALT | LINUX_REBOOT_CMD_POWER_OFF => {
            // TODO: flush/sync filesystems (e.g. sys_sync) before pulling the
            // plug; `shutdown()` never returns, so cleanup must happen first.
            warn!("reboot: initiating platform shutdown (command {command:#x})");
            khal::power::shutdown()
        }
        _ => Err(KError::InvalidInput),
    }
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
    let file = Filename::new(path).open_with_flags_at(
        fs.root(),
        fs.pwd(),
        0,
        NodePermission::empty(),
        NodePermission::empty(),
        kprocess::current_cred(),
    )?;
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
