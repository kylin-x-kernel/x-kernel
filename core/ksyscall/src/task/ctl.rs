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

use kerrno::{KError, KResult, LinuxError};
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
/// - PR_SET_SECCOMP: reject strict/filter installation until seccomp is enforced
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
/// Security restrictions are not successful no-ops: `PR_SET_SECCOMP` returns
/// `ENOSYS` for strict/filter mode and `EINVAL` for any other mode. The mode
/// retains its full `unsigned long` ABI width. No filter pointer is accessed
/// and no seccomp or no-new-privileges state is changed on this path.
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
            if !matches!(arg2, SECCOMP_MODE_STRICT | SECCOMP_MODE_FILTER) {
                return Err(KError::InvalidInput);
            }
            // Reporting success would let callers trust a restriction that
            // the syscall dispatch path does not enforce.
            return Err(KError::from(LinuxError::ENOSYS));
        }
        PR_GET_SECCOMP => {
            // Seccomp is not enforced, so mode 0 (disabled). Linux returns the
            // mode directly as the prctl return value and never writes user
            // memory: arg2..arg5 must all be zero, otherwise EINVAL
            // (kernel/sys.c), so this does not touch `arg2`.
            if arg2 != 0 || arg3 != 0 || arg4 != 0 || arg5 != 0 {
                return Err(KError::InvalidInput);
            }
            return Ok(0);
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
        // child subreaper semantics are not tracked.
        PR_GET_CHILD_SUBREAPER => {
            if arg2 != 0 {
                write_vm_mem(arg2 as _, &[0u32])?;
            }
        }
        PR_SET_CHILD_SUBREAPER => {}
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

#[unittest::mod_test]
mod tests {
    use kerrno::{KError, LinuxError};
    use linux_raw_sys::prctl::*;
    use unittest::{assert_eq, def_test};

    use super::{SECCOMP_MODE_FILTER, SECCOMP_MODE_STRICT, sys_prctl};

    #[def_test(user, serial)]
    fn test_prctl_keep_caps_set_get() {
        assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
        assert_eq!(sys_prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0), Ok(0));

        assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 1, 0, 0, 0), Ok(0));
        assert_eq!(sys_prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0), Ok(1));

        assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
    }

    #[def_test(user, serial)]
    fn test_prctl_keep_caps_rejects_invalid_value() {
        assert_eq!(sys_prctl(PR_SET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
        assert_eq!(
            sys_prctl(PR_SET_KEEPCAPS, 2, 0, 0, 0),
            Err(KError::InvalidInput)
        );
        assert_eq!(sys_prctl(PR_GET_KEEPCAPS, 0, 0, 0, 0), Ok(0));
    }

    #[def_test(user, serial)]
    fn prctl_set_seccomp_requires_enforcement() {
        let nnp_before = sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
        for mode in [SECCOMP_MODE_STRICT, SECCOMP_MODE_FILTER] {
            for filter_addr in [0, 1, usize::MAX] {
                assert_eq!(
                    sys_prctl(PR_SET_SECCOMP, mode, filter_addr, 0, 0),
                    Err(KError::from(LinuxError::ENOSYS))
                );
                assert_eq!(sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0), Ok(0));
                assert_eq!(sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), nnp_before);
            }
        }
    }

    #[def_test(user, serial)]
    fn prctl_set_seccomp_rejects_invalid_modes() {
        let nnp_before = sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0);
        for mode in [0, 3, 999, u32::MAX as usize, usize::MAX] {
            for filter_addr in [0, 1, usize::MAX] {
                assert_eq!(
                    sys_prctl(PR_SET_SECCOMP, mode, filter_addr, 0, 0),
                    Err(KError::InvalidInput)
                );
                assert_eq!(sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0), Ok(0));
                assert_eq!(sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), nnp_before);
            }
        }
    }

    #[def_test(user, serial)]
    fn prctl_set_seccomp_preserves_unsigned_long_mode_width() {
        // Unlike seccomp(2)'s operation, this argument must not truncate to u32.
        if let Some(high) = 1usize.checked_shl(32) {
            for mode in [high, high | SECCOMP_MODE_STRICT, high | SECCOMP_MODE_FILTER] {
                assert_eq!(
                    sys_prctl(PR_SET_SECCOMP, mode, usize::MAX, 0, 0),
                    Err(KError::InvalidInput)
                );
            }
        }
    }

    #[def_test(user, serial)]
    fn prctl_get_seccomp_returns_mode_zero_without_writing_arg2() {
        // Linux returns the seccomp mode as the prctl return value and never
        // writes user memory; with arg2..arg5 zero it reports mode 0.
        assert_eq!(sys_prctl(PR_GET_SECCOMP, 0, 0, 0, 0), Ok(0));
    }

    #[def_test(user, serial)]
    fn prctl_get_seccomp_rejects_nonzero_args_with_einval() {
        // Linux requires arg2..arg5 to be zero for PR_GET_SECCOMP.
        assert_eq!(
            sys_prctl(PR_GET_SECCOMP, 1, 0, 0, 0),
            Err(KError::InvalidInput)
        );
    }

    #[def_test(user, serial)]
    fn prctl_get_dumpable_reports_default() {
        assert_eq!(sys_prctl(PR_GET_DUMPABLE, 0, 0, 0, 0), Ok(1));
    }

    #[def_test(user, serial)]
    fn prctl_get_securebits_reports_unset() {
        assert_eq!(sys_prctl(PR_GET_SECUREBITS, 0, 0, 0, 0), Ok(0));
    }

    #[def_test(user, serial)]
    fn prctl_get_no_new_privs_reports_unset() {
        assert_eq!(sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), Ok(0));
    }

    #[def_test(user, serial)]
    fn prctl_set_no_new_privs_records_the_flag() {
        // Programs that require this prctl to succeed -- notably Chromium's
        // child-process launcher -- abort subprocess creation on failure, so
        // recording the flag must succeed and be observable via PR_GET.
        assert_eq!(sys_prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0), Ok(0));
        assert_eq!(sys_prctl(PR_GET_NO_NEW_PRIVS, 0, 0, 0, 0), Ok(1));
        // Linux rejects a non-zero argument tail with EINVAL.
        assert_eq!(
            sys_prctl(PR_SET_NO_NEW_PRIVS, 1, 1, 0, 0),
            Err(KError::InvalidInput)
        );
    }

    #[def_test(user, serial)]
    fn prctl_set_no_new_privs_rejects_invalid_args() {
        assert_eq!(
            sys_prctl(PR_SET_NO_NEW_PRIVS, 1, 1, 0, 0),
            Err(KError::InvalidInput)
        );
        assert_eq!(
            sys_prctl(PR_SET_NO_NEW_PRIVS, 2, 0, 0, 0),
            Err(KError::InvalidInput)
        );
    }

    #[def_test(user, serial)]
    fn prctl_set_pdeath_sig_validates_signal_range() {
        // Linux `valid_signal`: 0 (clear) and signals 1..=_NSIG (64, including
        // SIGRTMAX) are accepted.
        assert_eq!(sys_prctl(PR_SET_PDEATHSIG, 0, 0, 0, 0), Ok(0));
        assert_eq!(sys_prctl(PR_SET_PDEATHSIG, 9, 0, 0, 0), Ok(0));
        assert_eq!(sys_prctl(PR_SET_PDEATHSIG, 64, 0, 0, 0), Ok(0));
        // Out-of-range signals are rejected with EINVAL.
        assert_eq!(
            sys_prctl(PR_SET_PDEATHSIG, 65, 0, 0, 0),
            Err(KError::InvalidInput)
        );
    }

    #[def_test(user, serial)]
    fn prctl_get_thp_disable_reports_disabled() {
        assert_eq!(sys_prctl(PR_GET_THP_DISABLE, 0, 0, 0, 0), Ok(1));
    }

    #[def_test(user, serial)]
    fn prctl_get_timerslack_reports_default() {
        assert_eq!(sys_prctl(PR_GET_TIMERSLACK, 0, 0, 0, 0), Ok(50_000));
    }

    #[def_test(user, serial)]
    fn prctl_capbset_read_reports_in_set() {
        assert_eq!(sys_prctl(PR_CAPBSET_READ, 5, 0, 0, 0), Ok(1));
    }

    #[def_test(user, serial)]
    fn prctl_capbset_read_rejects_unknown_capability_with_einval() {
        assert_eq!(
            sys_prctl(PR_CAPBSET_READ, 9999, 0, 0, 0),
            Err(KError::InvalidInput)
        );
    }

    #[def_test(user, serial)]
    fn prctl_cap_ambient_is_set_reports_unset() {
        assert_eq!(
            sys_prctl(PR_CAP_AMBIENT, PR_CAP_AMBIENT_IS_SET as usize, 0, 0, 0),
            Ok(0)
        );
    }

    #[def_test(user, serial)]
    fn prctl_cap_ambient_rejects_unknown_subcommand_with_einval() {
        assert_eq!(
            sys_prctl(PR_CAP_AMBIENT, 0xFFFF, 0, 0, 0),
            Err(KError::InvalidInput)
        );
    }
}
