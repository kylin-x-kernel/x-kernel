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
use kprocess::current_user_process;
use ktime_types::TimeSpan;
use linux_raw_sys::{
    ctypes::c_char,
    general::{
        GRND_INSECURE, GRND_NONBLOCK, GRND_RANDOM, LINUX_REBOOT_CMD_CAD_OFF,
        LINUX_REBOOT_CMD_CAD_ON, LINUX_REBOOT_CMD_HALT, LINUX_REBOOT_CMD_POWER_OFF,
        LINUX_REBOOT_CMD_SW_SUSPEND, LINUX_REBOOT_MAGIC1, LINUX_REBOOT_MAGIC2,
        LINUX_REBOOT_MAGIC2A, LINUX_REBOOT_MAGIC2B, LINUX_REBOOT_MAGIC2C,
    },
    system::new_utsname,
};
use osvm::{VirtPtr, write_vm_mem};
use posix_types::{UserConstPtr, UserPtr, system::k_sysinfo};

// Re-export the architecture-specific syscalls (e.g. `sys_riscv_hwprobe` on
// riscv64) so dispatch can reach them via `crate::sys::*`. Gated to riscv64
// because the module is empty on other architectures, where a glob import
// would otherwise trip `-D unused-imports`.
#[cfg(target_arch = "riscv64")]
pub(crate) use crate::arch::*;

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
pub(crate) fn sys_uname(name: UserPtr<new_utsname>) -> KResult<isize> {
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

/// Convert elapsed boot time to Linux sysinfo seconds, rounding up.
///
/// Linux `do_sysinfo()` adds one second when the nanosecond part is nonzero.
/// Unlike `clock_gettime(CLOCK_BOOTTIME)`, this ABI has no nanosecond field:
/// its value can exceed `timespec.tv_sec` by one for the same instant.
/// See <https://github.com/torvalds/linux/blob/v6.6/kernel/sys.c#L2601-L2611>.
fn sysinfo_uptime_seconds(elapsed: TimeSpan) -> isize {
    elapsed
        .as_secs()
        .saturating_add(u64::from(elapsed.subsec_nanos() != 0))
        .min(isize::MAX as u64) as isize
}

/// Get general system information such as uptime, process count and memory unit
pub(crate) fn sys_sysinfo(info: UserPtr<k_sysinfo>) -> KResult<isize> {
    let mut kinfo = k_sysinfo::default();
    // Keep uptime on the same clock source as CLOCK_BOOTTIME.
    kinfo.uptime = sysinfo_uptime_seconds(ktime::boottime().span_since_origin()) as _;
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
pub(crate) fn sys_syslog(_type: i32, _buf: *mut c_char, _len: usize) -> KResult<isize> {
    Ok(0)
}

/// Sets the hostname in the calling process's UTS namespace.
pub(crate) fn sys_sethostname(name: UserConstPtr<u8>, len: usize) -> KResult<isize> {
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
/// `reboot(2)` ABI. `HALT`, `POWER_OFF`, the `CAD_ON`/`CAD_OFF` toggles,
/// and `SW_SUSPEND` are handled here; `HALT` stops all CPUs and keeps the
/// system powered, `POWER_OFF` cuts power through the platform power-off
/// agent, and `SW_SUSPEND` requests suspend-to-RAM through the platform
/// sleep agent, failing with the platform's error where no agent exists.
/// Other commands (`RESTART`, `RESTART2`, `KEXEC`) are rejected with
/// `EINVAL`, since `reboot(2)` returns `EINVAL` — not `ENOSYS` — for an
/// unsupported command.
pub(crate) fn sys_reboot(
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
        LINUX_REBOOT_CMD_HALT => {
            // TODO: flush/sync filesystems before the terminal; the final
            // orderly-shutdown supervisor owns that cleanup, and
            // `khal::power::halt()` never returns.
            warn!("reboot: halting system (command {command:#x})");
            khal::power::halt()
        }
        LINUX_REBOOT_CMD_POWER_OFF => {
            // TODO: flush/sync filesystems before the terminal; the final
            // orderly-shutdown supervisor owns that cleanup, and
            // `khal::power::power_off()` never returns.
            warn!("reboot: powering off system (command {command:#x})");
            khal::power::power_off()
        }
        LINUX_REBOOT_CMD_SW_SUSPEND => {
            // TODO: flush/sync filesystems and quiesce devices before
            // suspending; the orderly-shutdown supervisor owns that
            // cleanup. A successful return means the platform slept and
            // was resumed.
            warn!("reboot: suspending to RAM (command {command:#x})");
            khal::power::suspend_to_ram()?;
            Ok(0)
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

/// Linux getrandom(2) single-call caps from the man-page notes.
const GETRANDOM_MAX_URANDOM: usize = (32 << 20) - 1; // 32 MiB - 1
const GETRANDOM_MAX_RANDOM: usize = 512;
/// Kernel scratch buffer size; avoids a single allocation up to the urandom cap.
const GETRANDOM_CHUNK: usize = 64 * 1024;

/// Get random bytes from the kernel entropy pool (`getrandom(2)`).
///
/// - Unknown `flags` bits, and `GRND_INSECURE|GRND_RANDOM`, return `EINVAL`.
/// - Without `GRND_INSECURE`, the pool must be quality-seeded ([`entropy::is_ready`]).
/// - With `GRND_NONBLOCK` and an unready pool, returns `EAGAIN` (`WouldBlock`).
/// - Without `GRND_NONBLOCK`, blocks until the pool is ready (via
///   [`entropy::wait_until_ready`]) before returning bytes.
/// - `GRND_RANDOM` is accepted for ABI compatibility; output still comes from
///   the same ChaCha20 pool as the default path.
/// - Request length is clamped to the Linux single-call limits (`512` with
///   `GRND_RANDOM`, otherwise `32 MiB - 1`) and copied in chunks.
///
/// # Arguments
///
/// `buf` is a user virtual address in the current address space; `len` is its
/// requested capacity in bytes. `flags` contains Linux `GRND_*` bits. Requires
/// initialized entropy, allocator and user-memory services, and a sleepable
/// thread context for the blocking path.
///
/// # Returns
///
/// Returns the clamped byte count, or zero for a zero-length request after flag
/// validation. `GRND_INSECURE` permits output before quality seeding.
///
/// # Errors
///
/// Returns `InvalidInput` for invalid flags, `WouldBlock` when nonblocking secure
/// output is not ready, or the user-memory copy error. If a later chunk faults,
/// earlier chunks remain written but the call returns the error, not a short count.
///
/// # Panics
///
/// Scratch-buffer allocation failure follows kernel allocator policy.
pub fn sys_getrandom(buf: *mut u8, len: usize, flags: u32) -> KResult<isize> {
    // Reject unknown bits and the nonsensical INSECURE|RANDOM combination
    // (Linux getrandom(2) → EINVAL).
    let flags = GetRandomFlags::from_bits(flags).ok_or(KError::InvalidInput)?;
    if flags.contains(GetRandomFlags::INSECURE) && flags.contains(GetRandomFlags::RANDOM) {
        return Err(KError::InvalidInput);
    }

    if len == 0 {
        return Ok(0);
    }

    debug!("sys_getrandom <= buf: {buf:p}, len: {len}, flags: {flags:?}");

    let insecure = flags.contains(GetRandomFlags::INSECURE);
    if !insecure && !entropy::is_ready() {
        entropy::try_seed_from_hardware();
        if !entropy::is_ready() {
            if flags.contains(GetRandomFlags::NONBLOCK) {
                return Err(KError::WouldBlock);
            }
            // Block until quality entropy is mixed; do not return bootstrap-only
            // ChaCha output on the secure path.
            entropy::wait_until_ready();
        }
    }

    let max_len = if flags.contains(GetRandomFlags::RANDOM) {
        GETRANDOM_MAX_RANDOM
    } else {
        GETRANDOM_MAX_URANDOM
    };
    let len = len.min(max_len);

    let mut kbuf = alloc::vec![0u8; len.min(GETRANDOM_CHUNK)];
    let mut written = 0usize;
    while written < len {
        let chunk = (len - written).min(kbuf.len());
        let slice = &mut kbuf[..chunk];
        entropy::fill_random(slice);
        write_vm_mem(buf.wrapping_add(written), slice)?;
        written += chunk;
    }

    Ok(written as _)
}
unittest::path_mod_test!(tests_sysinfo, "../ut-tests/sys/tests_sysinfo.rs");
