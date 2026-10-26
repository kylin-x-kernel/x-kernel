// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Process capability and control syscalls.
//!
//! This module implements process control and capability operations including:
//! - Process capabilities (capget, capset, etc.)
//! - Process resource limits (prlimit, etc.)
//! - Process information queries

use core::ffi::c_char;

use kerrno::{KError, KResult};
use ksignal::MAX_SIGNALS;
use ktask::current;
use kuaccess::vm_load_string;
use linux_raw_sys::general::{__user_cap_data_struct, __user_cap_header_struct};
use osvm::write_vm_mem;
use posix_types::UserPtr;

const CAPABILITY_VERSION_3: u32 = 0x20080522;
const CAP_LAST_CAP: usize = 40;
const SECCOMP_MODE_STRICT: usize = 1;
const SECCOMP_MODE_FILTER: usize = 2;

fn validate_cap_header(header_ptr: UserPtr<__user_cap_header_struct>) -> KResult<()> {
    let mut header = header_ptr.read_vm()?;
    if header.version != CAPABILITY_VERSION_3 {
        header.version = CAPABILITY_VERSION_3;
        header_ptr.write_vm(header)?;
        return Err(KError::InvalidInput);
    }
    kprocess::capability::validate_target_pid(header.pid as u32)?;
    Ok(())
}

pub(crate) fn sys_capget(
    header: UserPtr<__user_cap_header_struct>,
    data: UserPtr<__user_cap_data_struct>,
) -> KResult<isize> {
    validate_cap_header(header)?;

    data.write_vm(__user_cap_data_struct {
        effective: u32::MAX,
        permitted: u32::MAX,
        inheritable: u32::MAX,
    })?;
    Ok(0)
}

pub(crate) fn sys_capset(
    header: UserPtr<__user_cap_header_struct>,
    _data: UserPtr<__user_cap_data_struct>,
) -> KResult<isize> {
    validate_cap_header(header)?;

    Ok(0)
}

pub(crate) fn sys_get_mempolicy(
    _policy: *mut i32,
    _nodemask: *mut usize,
    _maxnode: usize,
    _addr: usize,
    _flags: usize,
) -> KResult<isize> {
    warn!("Dummy get_mempolicy called");
    Ok(0)
}

/// prctl() is called with a first argument describing what to do, and further
/// arguments with a significance depending on the first one.
/// The first argument can be:
/// - PR_SET_NAME: set the name of the calling thread, using the value pointed to by `arg2`
/// - PR_GET_NAME: get the name of the calling
/// - PR_SET_SECCOMP: install strict/filter mode through the `seccomp(2)` path
/// - PR_CAPBSET_READ: return whether a capability is in the bounding set
/// - PR_GET_KEEPCAPS / PR_SET_KEEPCAPS: query or set the keep-capabilities flag
/// - PR_MCE_KILL: set the machine check exception policy
/// - PR_SET_MM options: set various memory management options (start/end code/data/brk/stack)
///
/// Many options below are answered with static defaults: this kernel has no
/// dumpable flag, no securebits, and runs every task with full capabilities, so
/// getters return their "fully privileged, nothing set" value while setters are
/// accepted as no-ops. This matches the behavior user space expects from a
/// Linux-like kernel on these (often merely probed) options, and avoids noisy
/// warnings for each unimplemented option.
///
/// Security restrictions are not successful no-ops: `PR_SET_SECCOMP` is the
/// `prctl` spelling of the `seccomp(2)` install path (`arg2` is the mode with
/// its full `unsigned long` ABI width, `arg3` the optional filter program,
/// which strict mode ignores), and `PR_GET_SECCOMP` reports the thread's real
/// mode. Any other mode value is `EINVAL`.
#[expect(
    clippy::too_many_lines,
    reason = "CG-028-focused-functions: prctl dispatches many unrelated Linux operations; \
              splitting the match would scatter the shared errno policy"
)]
pub(crate) fn sys_prctl(
    option: u32,
    arg2: usize,
    arg3: usize,
    arg4: usize,
    arg5: usize,
) -> KResult<isize> {
    use linux_raw_sys::prctl::*;

    debug!("sys_prctl <= option: {option}, args: {arg2}, {arg3}, {arg4}, {arg5}");

    match option {
        PR_SET_NAME => {
            let s = vm_load_string(arg2 as *const c_char)?;
            current().set_name(&s);
        }
        PR_GET_NAME => {
            let name = current().name();
            let len = name.len().min(15);
            let mut buf = [0; 16];
            buf[..len].copy_from_slice(&name.as_bytes()[..len]);
            write_vm_mem(arg2 as _, &buf)?;
        }
        PR_SET_SECCOMP => {
            // Linux `prctl_set_seccomp` is the `seccomp(2)` install path with
            // no flags. The mode keeps `unsigned long` width, and strict mode
            // always ignores the filter pointer.
            let op = match arg2 {
                SECCOMP_MODE_STRICT => linux_raw_sys::ptrace::SECCOMP_SET_MODE_STRICT,
                SECCOMP_MODE_FILTER => linux_raw_sys::ptrace::SECCOMP_SET_MODE_FILTER,
                _ => return Err(KError::InvalidInput),
            };
            let args = if arg2 == SECCOMP_MODE_STRICT {
                core::ptr::null()
            } else {
                arg3 as *const ()
            };
            crate::task::sys_seccomp(op, 0, args)?;
        }
        PR_GET_SECCOMP => {
            // Linux `prctl_get_seccomp` returns the thread's real mode as the
            // return value and never writes user memory; arg2..arg5 must all be
            // zero, otherwise EINVAL (kernel/sys.c).
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(KError::InvalidInput);
            }
            return Ok(kprocess::current_user_thread().seccomp().linux_mode() as isize);
        }
        PR_GET_PDEATHSIG => {
            // Linux kernel/sys.c: `put_user(me->pdeath_signal, (int __user *)arg2)`.
            let signal = kprocess::current_user_process().pdeath_signal();
            write_vm_mem(arg2 as _, &[signal])?;
        }
        PR_SET_PDEATHSIG => {
            // Linux `valid_signal(arg2)`: 0 clears, otherwise 1..=_NSIG (64,
            // including SIGRTMAX).
            if arg2 > MAX_SIGNALS {
                return Err(KError::InvalidInput);
            }
            kprocess::current_user_process().set_pdeath_signal(arg2 as u32);
        }
        PR_SET_NO_NEW_PRIVS => {
            if arg2 != 1 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(KError::InvalidInput);
            }
            // The exec credential path consumes this flag via
            // `Cred::apply_exec`; see docs/security.md for the
            // no-elevation contract honored on exec.
            kprocess::current_user_thread().set_no_new_privileges();
            return Ok(0);
        }
        PR_GET_NO_NEW_PRIVS => {
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(KError::InvalidInput);
            }
            return Ok(kprocess::current_user_thread().no_new_privileges() as isize);
        }
        PR_CAPBSET_READ => {
            if arg2 > CAP_LAST_CAP {
                return Err(KError::InvalidInput);
            }
            return Ok(1);
        }
        PR_CAPBSET_DROP => {}
        PR_GET_KEEPCAPS => {
            return Ok(isize::from(kprocess::current_cred().keep_caps()));
        }
        PR_SET_KEEPCAPS => {
            // arg2 must be 0/1; refuse when locked; persist on the credential.
            if arg2 > 1 {
                return Err(KError::InvalidInput);
            }
            if arg2 != 0 {
                super::credentials::update_current_cred(|cred| cred.keep_caps_enable())?;
            } else {
                super::credentials::update_current_cred(|cred| cred.keep_caps_disable())?;
            }
        }
        PR_MCE_KILL => {}
        // We do not track the process dumpable flag; report the Linux default
        // (`SUID_DUMP_USER` semantics observed by user space as dumpable=1).
        PR_GET_DUMPABLE => return Ok(1),
        PR_SET_DUMPABLE => {}
        // securebits are not tracked; nothing is set.
        PR_GET_SECUREBITS => return Ok(0),
        PR_SET_SECUREBITS => {}
        // Child subreaper: orphaned descendants reparent to the nearest
        // subreaper ancestor instead of the child reaper; container runtimes
        // (containerd shims) rely on this to observe container process exits.
        PR_GET_CHILD_SUBREAPER => {
            if arg2 != 0 {
                let flag = kprocess::current_user_process().is_child_subreaper() as u32;
                write_vm_mem(arg2 as _, &[flag])?;
            }
        }
        PR_SET_CHILD_SUBREAPER => {
            kprocess::current_user_process().set_child_subreaper(arg2 != 0);
        }
        // We do not expose a separate tid_address; report none.
        PR_GET_TID_ADDRESS => {
            if arg2 != 0 {
                write_vm_mem(arg2 as _, &[0usize])?;
            }
        }
        // Transparent hugepages are not supported; report disabled.
        PR_GET_THP_DISABLE => return Ok(1),
        PR_SET_THP_DISABLE => {}
        // timerslack is not tracked; report the kernel default.
        PR_GET_TIMERSLACK => return Ok(50_000),
        PR_SET_TIMERSLACK => {}
        PR_CAP_AMBIENT => {
            // The ambient capability set is not tracked. Only the read
            // sub-action produces a value; raise/lower/clear are accepted as
            // no-ops since we always operate with full effective capabilities.
            match arg2 as u32 {
                PR_CAP_AMBIENT_IS_SET => return Ok(0),
                PR_CAP_AMBIENT_RAISE | PR_CAP_AMBIENT_LOWER | PR_CAP_AMBIENT_CLEAR_ALL => {}
                _ => return Err(KError::InvalidInput),
            }
        }
        PR_SET_VMA => {
            // Allow user space to set anonymous VMA names (e.g. Go runtime).
            // We currently do not persist VMA metadata, but returning success
            // keeps behavior compatible with Linux for this common path.
            if arg2 as u32 != PR_SET_VMA_ANON_NAME {
                return Err(KError::InvalidInput);
            }
        }
        PR_SET_MM => {
            // not implemented; but avoid annoying warnings
            return Err(KError::InvalidInput);
        }
        _ => {
            warn!("sys_prctl: unsupported option {option}");
            return Err(KError::InvalidInput);
        }
    }

    Ok(0)
}

unittest::path_mod_test!(tests, "../../ut-tests/task/ctl/tests.rs");
