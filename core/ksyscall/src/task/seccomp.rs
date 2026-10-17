// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! `seccomp(2)` syscall: strict mode, filter loading, action queries.

use alloc::{sync::Arc, vec::Vec};
use core::mem::{MaybeUninit, size_of};

use kerrno::{KError, KResult, LinuxError};
use kprocess::{AsThread, CurrentThread, current_user_thread};
use kseccomp::{SeccompFilter, SeccompFilterError, SockFilter};
use linux_raw_sys::ptrace::{
    SECCOMP_FILTER_FLAG_LOG, SECCOMP_FILTER_FLAG_SPEC_ALLOW, SECCOMP_FILTER_FLAG_TSYNC,
    SECCOMP_FILTER_FLAG_TSYNC_ESRCH, SECCOMP_GET_ACTION_AVAIL, SECCOMP_GET_NOTIF_SIZES,
    SECCOMP_RET_ALLOW, SECCOMP_RET_ERRNO, SECCOMP_RET_KILL_PROCESS, SECCOMP_RET_KILL_THREAD,
    SECCOMP_RET_LOG, SECCOMP_RET_TRACE, SECCOMP_RET_TRAP, SECCOMP_RET_USER_NOTIF,
    SECCOMP_SET_MODE_FILTER, SECCOMP_SET_MODE_STRICT,
};
use osvm::read_vm_bytes;

/// Bitmask of `seccomp` flags accepted by [`sys_seccomp`].
///
/// Linux rejects every flag outside this whitelist with `EINVAL` rather
/// than silently ignoring it. `TSYNC_ESRCH` is accepted functionally even
/// though thread-group synchronization currently targets live threads
/// (the ESRCH nuance applies when the caller's own thread group is not
/// found, which cannot arise for the invoking thread).
const SECCOMP_ALLOWED_FLAGS: u32 = SECCOMP_FILTER_FLAG_TSYNC
    | SECCOMP_FILTER_FLAG_TSYNC_ESRCH
    | SECCOMP_FILTER_FLAG_LOG
    | SECCOMP_FILTER_FLAG_SPEC_ALLOW;

/// Size in bytes of `struct sock_fprog` (`u16 len` + pointer).
const SOCK_FPROG_SIZE: usize = 16;
/// Size in bytes of `struct sock_filter` (`u16` + two `u8` + `u32`).
const SOCK_FILTER_SIZE: usize = 8;

/// `seccomp(2)` entry point.
///
/// # Errors
///
/// Returns `EINVAL` for unsupported operations/flags, a malformed program, or
/// a mode change Linux refuses, `EFAULT` when the `sock_fprog` or instruction
/// array is unreadable, `EACCES` for a filter install by a caller that is
/// neither privileged nor has set `PR_SET_NO_NEW_PRIVS` (Linux
/// `seccomp_prepare_filter`; strict mode is not gated), and `ENOSYS` for
/// operations this kernel does not implement (e.g. user notifications).
pub(crate) fn sys_seccomp(op: u32, flags: u32, args: *const ()) -> KResult<isize> {
    match op {
        SECCOMP_SET_MODE_STRICT => set_strict(flags),
        SECCOMP_SET_MODE_FILTER => set_filter(flags, args),
        SECCOMP_GET_ACTION_AVAIL => get_action_avail(flags, args),
        SECCOMP_GET_NOTIF_SIZES => Err(KError::from(LinuxError::ENOSYS)),
        _ => Err(KError::InvalidInput),
    }
}

fn set_strict(flags: u32) -> KResult<isize> {
    if flags != 0 {
        return Err(KError::InvalidInput);
    }
    // Linux `seccomp_set_mode_strict` has no privilege requirement: strict mode
    // only ever removes syscalls, so an unprivileged task may sandbox itself.
    current_user_thread().seccomp().set_strict()?;
    Ok(0)
}

fn set_filter(flags: u32, args: *const ()) -> KResult<isize> {
    if flags & !SECCOMP_ALLOWED_FLAGS != 0 {
        return Err(KError::InvalidInput);
    }
    // A filter install is gated (Linux `seccomp_prepare_filter`); strict mode
    // is not.
    require_install_privilege()?;
    if args.is_null() {
        return Err(KError::InvalidInput);
    }

    let (len, filter_ptr) = read_sock_fprog(args)?;
    // Bound the length before allocating; Linux caps filters at
    // `BPF_MAXINSNS`, and a huge len would otherwise cause a spurious
    // allocation.
    if len > kseccomp::BPF_MAXINSNS {
        return Err(KError::InvalidInput);
    }
    let instructions = read_instructions(filter_ptr, len)?;
    let filter = Arc::new(SeccompFilter::new(instructions).map_err(map_filter_error)?);

    let thread = current_user_thread();

    // Linux validates every sibling before installing on the caller, so a
    // refused `TSYNC` leaves no thread (including the caller) modified.
    if flags & SECCOMP_FILTER_FLAG_TSYNC != 0
        && let Some(blocking_tid) = first_unsyncable_sibling(&thread)
    {
        return if flags & SECCOMP_FILTER_FLAG_TSYNC_ESRCH != 0 {
            Err(KError::from(LinuxError::ESRCH))
        } else {
            // Linux returns the offending thread's pid as the result.
            Ok(blocking_tid as isize)
        };
    }

    thread.seccomp().append_filter(filter)?;

    if flags & SECCOMP_FILTER_FLAG_TSYNC != 0 {
        sync_to_thread_group(&thread);
    }

    Ok(0)
}

/// Rejects a *filter* install from a caller without `CAP_SYS_ADMIN` unless it
/// has set `PR_SET_NO_NEW_PRIVS` (Linux `seccomp_prepare_filter`).
///
/// Strict mode is not gated: Linux `seccomp_set_mode_strict` only checks that
/// the mode may be assigned.
fn require_install_privilege() -> KResult<()> {
    if current_user_thread().no_new_privileges() || current_cred_privileged() {
        Ok(())
    } else {
        Err(KError::from(LinuxError::EACCES))
    }
}

fn map_filter_error(_error: SeccompFilterError) -> KError {
    // Every rejection is an install-time `EINVAL` in Linux.
    KError::InvalidInput
}

fn current_cred_privileged() -> bool {
    kprocess::current_cred().is_privileged()
}

/// Copies `N` bytes from user memory into an initialised array.
///
/// The ABI structs here are decoded field-wise instead of reinterpreting a
/// `#[repr(C)]` image, so no alignment is imposed on the user pointer (Linux
/// `copy_from_user` accepts unaligned addresses) and the byte layout stays
/// explicit.
fn read_user_bytes<const N: usize>(addr: *const u8) -> KResult<[u8; N]> {
    let mut buf = [MaybeUninit::<u8>::uninit(); N];
    read_vm_bytes(addr, &mut buf)?;
    // SAFETY: `read_vm_bytes` returned `Ok`, so it initialised all `N` bytes.
    Ok(buf.map(|byte| unsafe { byte.assume_init() }))
}

/// Reads `struct sock_fprog { u16 len; struct sock_filter *filter; }`.
fn read_sock_fprog(args: *const ()) -> KResult<(usize, usize)> {
    let bytes: [u8; SOCK_FPROG_SIZE] = read_user_bytes(args as *const u8)?;
    let len = u16::from_le_bytes([bytes[0], bytes[1]]) as usize;
    let mut pointer_bytes = [0u8; SOCK_FPROG_SIZE - 8];
    pointer_bytes.copy_from_slice(&bytes[8..]);
    Ok((len, usize::from_le_bytes(pointer_bytes)))
}

/// Reads and field-decodes the caller's `struct sock_filter` array.
fn read_instructions(base: usize, len: usize) -> KResult<Vec<SockFilter>> {
    let mut instructions = Vec::with_capacity(len);
    for index in 0..len {
        let offset = index
            .checked_mul(SOCK_FILTER_SIZE)
            .and_then(|offset| base.checked_add(offset))
            .ok_or(KError::InvalidInput)?;
        let bytes: [u8; SOCK_FILTER_SIZE] = read_user_bytes(offset as *const u8)?;
        instructions.push(SockFilter {
            code: u16::from_le_bytes([bytes[0], bytes[1]]),
            jt: bytes[2],
            jf: bytes[3],
            k: u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        });
    }
    Ok(instructions)
}

/// Returns the first sibling that cannot adopt `caller`'s chain, if any.
///
/// Mirrors Linux `seccomp_can_sync_threads`: the caller is told which thread
/// blocked the synchronization.
fn first_unsyncable_sibling(caller: &CurrentThread) -> Option<u32> {
    let process = caller.process();
    let caller_context = caller.seccomp();
    for task in kprocess::scheduler::process_tasks(process.as_ref()) {
        let thread = task.as_thread();
        if thread.tid() == caller.tid() {
            continue;
        }
        if !thread.seccomp().is_syncable_with(caller_context) {
            return Some(thread.tid());
        }
    }
    None
}

/// Applies the caller's chain to every sibling (Linux
/// `SECCOMP_FILTER_FLAG_TSYNC`).
///
/// Only called after [`first_unsyncable_sibling`] accepted every sibling, so
/// this can only add restrictions.
fn sync_to_thread_group(caller: &CurrentThread) {
    let process = caller.process();
    let caller_context = caller.seccomp();
    // Linux also propagates no-new-privileges, so an unprivileged task cannot
    // shed the restriction by handing seccomp to a short-lived sibling.
    let propagate_no_new_privileges = caller.no_new_privileges();
    for task in kprocess::scheduler::process_tasks(process.as_ref()) {
        let thread = task.as_thread();
        if thread.tid() == caller.tid() {
            continue;
        }
        // `sync_from` re-checks under the sibling's own lock: a sibling that
        // installed a stricter chain since the pre-check keeps it.
        if !thread.seccomp().sync_from(caller_context) {
            warn!(
                "seccomp TSYNC: thread {} kept a stricter chain installed during the sync",
                thread.tid()
            );
        }
        if propagate_no_new_privileges {
            thread.set_no_new_privileges();
        }
    }
}

/// Implements `SECCOMP_GET_ACTION_AVAIL`: reports whether the kernel
/// recognizes and can execute an action constant.
///
/// Linux accepts `ALLOW`, `KILL_THREAD`, `KILL_PROCESS`, `TRAP`, `ERRNO`,
/// `LOG`, `TRACE` and `USER_NOTIF`. Actions this kernel cannot execute
/// (`TRACE`, `USER_NOTIF`) return `ENOSYS`, and anything outside the set
/// returns `EINVAL`, mirroring the `seccomp_get_action_avail` behavior.
fn get_action_avail(flags: u32, args: *const ()) -> KResult<isize> {
    if flags != 0 || args.is_null() {
        return Err(KError::InvalidInput);
    }
    let bytes: [u8; 4] = read_user_bytes(args as *const u8)?;
    let action = u32::from_le_bytes(bytes);

    let executable = matches!(
        action,
        SECCOMP_RET_ALLOW
            | SECCOMP_RET_KILL_THREAD
            | SECCOMP_RET_KILL_PROCESS
            | SECCOMP_RET_TRAP
            | SECCOMP_RET_ERRNO
            | SECCOMP_RET_LOG
    );
    if executable {
        return Ok(0);
    }
    let recognized = matches!(action, SECCOMP_RET_TRACE | SECCOMP_RET_USER_NOTIF);
    if recognized {
        return Err(KError::from(LinuxError::ENOSYS));
    }
    Err(KError::InvalidInput)
}

/// Compile-time check that the field-wise decoding above matches the UAPI
/// struct sizes.
const _: () = {
    assert!(size_of::<linux_raw_sys::ptrace::sock_filter>() == SOCK_FILTER_SIZE);
    assert!(size_of::<linux_raw_sys::ptrace::sock_fprog>() == SOCK_FPROG_SIZE);
};
