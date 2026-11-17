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

use kcred::{CAP_LAST_CAP, CapSet, CapSets};
use kerrno::{KError, KResult};
use ksignal::MAX_SIGNALS;
use ktask::current;
use kuaccess::vm_load_string;
use linux_raw_sys::general::{__user_cap_data_struct, __user_cap_header_struct};
use osvm::write_vm_mem;
use posix_types::{UserPtr, task::CapUserData};
const SECCOMP_MODE_STRICT: usize = 1;
const SECCOMP_MODE_FILTER: usize = 2;

const CAPABILITY_VERSION_1: u32 = 0x19980330;
const CAPABILITY_VERSION_2: u32 = 0x20071026;
const CAPABILITY_VERSION_3: u32 = 0x20080522;

/// The capability data layout selected by the caller's header version.
///
/// Linux keeps accepting the two legacy versions: version 1 carries a single
/// 32-bit data word, while version 2 already uses the two-word layout and is
/// otherwise equivalent to version 3. Only version 1 needs a narrower copy;
/// Linux merely warns about the deprecated version-2 spelling.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum CapabilityVersion {
    /// One `__user_cap_data_struct` word (capabilities `0..31`).
    V1,
    /// Two `__user_cap_data_struct` words (capabilities `0..63`).
    V2,
}

/// Reads and validates the version word of a `capget(2)`/`capset(2)` header.
///
/// A version mismatch rewrites only the version word with the version this
/// kernel implements and fails with `EINVAL`, matching Linux
/// `cap_validate_magic()`. The data array and the header's PID word are
/// deliberately not touched on that path.
fn validate_cap_version(
    header_ptr: UserPtr<__user_cap_header_struct>,
) -> KResult<CapabilityVersion> {
    let version = header_ptr.cast::<u32>().read_vm()?;
    match version {
        CAPABILITY_VERSION_1 => Ok(CapabilityVersion::V1),
        CAPABILITY_VERSION_2 | CAPABILITY_VERSION_3 => Ok(CapabilityVersion::V2),
        _ => {
            // Linux `cap_validate_magic()` uses `put_user()` on the version
            // field, so a concurrent PID update in the second word is not
            // overwritten.
            header_ptr.cast::<u32>().write_vm(CAPABILITY_VERSION_3)?;
            Err(KError::InvalidInput)
        }
    }
}

/// Reads a complete capability header after validating its version.
///
/// The header carries the caller's target PID for the syscall-specific target
/// policy. `capget` splits this into a version probe and a later full read when
/// its data pointer is null, matching Linux's early return for that legacy
/// probe.
fn read_cap_header(
    header_ptr: UserPtr<__user_cap_header_struct>,
) -> KResult<(__user_cap_header_struct, CapabilityVersion)> {
    let version = validate_cap_version(header_ptr)?;
    Ok((header_ptr.read_vm()?, version))
}

fn cap_data_from_sets(caps: CapSets) -> CapUserData {
    CapUserData {
        low: __user_cap_data_struct {
            effective: caps.effective().low_word(),
            permitted: caps.permitted().low_word(),
            inheritable: caps.inheritable().low_word(),
        },
        high: __user_cap_data_struct {
            effective: caps.effective().high_word(),
            permitted: caps.permitted().high_word(),
            inheritable: caps.inheritable().high_word(),
        },
    }
}

fn cap_set_from_words(low: u32, high: u32) -> CapSet {
    CapSet::from_words(low, high).intersection(CapSet::FULL)
}

fn cap_sets_from_data(data: CapUserData) -> CapSets {
    // Linux `mk_kernel_cap()` masks each incoming word with `CAP_VALID_MASK`
    // before `security_capset()` validates the transition. Do the same here so
    // unknown bits neither enter the credential nor make an otherwise valid
    // `capset` fail the permitted-set subset rule.
    CapSets::new(
        cap_set_from_words(data.low.effective, data.high.effective),
        cap_set_from_words(data.low.permitted, data.high.permitted),
        cap_set_from_words(data.low.inheritable, data.high.inheritable),
    )
}

pub(crate) fn sys_capget(
    header: UserPtr<__user_cap_header_struct>,
    data: UserPtr<__user_cap_data_struct>,
) -> KResult<isize> {
    let version = match validate_cap_version(header) {
        Ok(version) => version,
        // Linux returns success for a null-data version probe even when the
        // version was unknown; it has already rewritten the version word.
        Err(err) => {
            return if data.is_null() && err == KError::InvalidInput {
                Ok(0)
            } else {
                Err(err)
            };
        }
    };
    // A null data pointer is Linux's legacy "which version do you implement?"
    // probe. It returns after version validation without reading the PID.
    if data.is_null() {
        return Ok(0);
    }

    let header = header.read_vm()?;
    if header.pid < 0 {
        return Err(KError::InvalidInput);
    }
    // PID zero and the caller's own TID both select the caller. All targets are
    // reported from objective credentials, matching Linux `cap_capget()`, which
    // reads `__task_cred()` even for the `current` special case.
    let observer = kprocess::current_user_thread()
        .process()
        .active_pid_namespace();
    let caps = kprocess::capability::target_capabilities(header.pid as u32, &observer)?;

    let caps = cap_data_from_sets(caps);
    match version {
        CapabilityVersion::V1 => data.write_vm(caps.low)?,
        CapabilityVersion::V2 => data.cast::<CapUserData>().write_vm(caps)?,
    }
    Ok(0)
}

/// Returns whether a `capset` target names the calling thread.
///
/// Linux compares the target with `task_pid_vnr(current)`. This kernel gives
/// process leaders a full projection in their active PID namespace but
/// allocates member-thread identities with only a root projection, so
/// `gettid(2)` reports the root TID even inside a nested PID namespace.
/// Both that reported TID and the active-namespace leader number reported by
/// `getpid(2)` name the caller and must be accepted. No other task's PID is
/// ever accepted because the comparison uses only the caller's own numbers.
fn capset_targets_caller(pid: i32, active_nr: u32, reported_tid: u32) -> bool {
    pid == 0 || (pid >= 0 && (pid as u32 == active_nr || pid as u32 == reported_tid))
}

pub(crate) fn sys_capset(
    header: UserPtr<__user_cap_header_struct>,
    data: UserPtr<__user_cap_data_struct>,
) -> KResult<isize> {
    let (header, version) = read_cap_header(header)?;

    // Linux credentials are per-thread and `capset` may modify only the
    // calling thread, not its whole thread group. The target check precedes
    // the data copy so every foreign PID, including a negative one, is
    // rejected with `EPERM` regardless of the caller's data pointer.
    let current_thread = kprocess::current_user_thread();
    if !capset_targets_caller(
        header.pid,
        current_thread.task_number().active_nr(),
        current_thread.tid(),
    ) {
        return Err(KError::OperationNotPermitted);
    }

    let caps = match version {
        CapabilityVersion::V1 => CapUserData {
            low: data.read_vm()?,
            high: __user_cap_data_struct {
                effective: 0,
                permitted: 0,
                inheritable: 0,
            },
        },
        CapabilityVersion::V2 => data.cast::<CapUserData>().read_vm()?,
    };
    let caps = cap_sets_from_data(caps);
    super::credentials::update_current_cred(|cred| cred.set_capabilities(caps))?;
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
/// - PR_CAPBSET_READ / PR_CAPBSET_DROP: query or drop a bounding capability
/// - PR_CAP_AMBIENT: inspect or modify the calling credential's ambient set
/// - PR_GET_KEEPCAPS / PR_SET_KEEPCAPS: query or set the keep-capabilities flag
/// - PR_GET_SECUREBITS / PR_SET_SECUREBITS: query or replace Linux secure-bits
/// - PR_MCE_KILL: set the machine check exception policy
/// - PR_SET_MM options: set various memory management options (start/end code/data/brk/stack)
///
/// Many options below are answered with static defaults: this kernel has no
/// dumpable flag. Capability, ambient-capability and secure-bits state is
/// tracked per credential. Getters for unimplemented state return their
/// Linux default while setters are accepted as no-ops. This matches the behavior
/// user space expects from a Linux-like kernel on these (often merely probed)
/// options, and avoids noisy warnings for each unimplemented option.
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
            if arg2 > CAP_LAST_CAP as usize {
                return Err(KError::InvalidInput);
            }
            let held = kprocess::current_cred()
                .cap_bounding()
                .contains(arg2 as u32);
            return Ok(isize::from(held));
        }
        PR_CAPBSET_DROP => {
            // `drop_bounding_cap()` owns the `CAP_SETPCAP` check and its order
            // relative to Linux's capability-number range validation.
            super::credentials::update_current_cred(|cred| cred.drop_bounding_cap(arg2))?;
        }
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
        PR_GET_SECUREBITS => return Ok(kprocess::current_cred().securebits() as isize),
        PR_SET_SECUREBITS => {
            super::credentials::update_current_cred(|cred| cred.set_securebits(arg2))?;
        }
        // Child subreaper: orphaned descendants reparent to the nearest
        // subreaper ancestor instead of the child reaper; container runtimes
        // (containerd shims) rely on this to observe container process exits.
        PR_GET_CHILD_SUBREAPER => {
            if arg2 != 0 {
                let flag = kprocess::current_user_process().is_child_subreaper() as u32;
                UserPtr::<u32>::from(arg2).write_vm(flag)?;
            }
        }
        PR_SET_CHILD_SUBREAPER => {
            kprocess::current_user_process().set_child_subreaper(arg2 != 0);
        }
        // We do not expose a separate tid_address; report none.
        PR_GET_TID_ADDRESS => {
            if arg2 != 0 {
                UserPtr::<usize>::from(arg2).write_vm(0)?;
            }
        }
        // Transparent hugepages are not supported; report disabled.
        PR_GET_THP_DISABLE => return Ok(1),
        PR_SET_THP_DISABLE => {}
        // timerslack is not tracked; report the kernel default.
        PR_GET_TIMERSLACK => return Ok(50_000),
        PR_SET_TIMERSLACK => {}
        PR_CAP_AMBIENT => {
            let subcommand = u32::try_from(arg2).map_err(|_| KError::InvalidInput)?;
            if subcommand == PR_CAP_AMBIENT_CLEAR_ALL {
                if arg3 != 0 || arg4 != 0 || arg5 != 0 {
                    return Err(KError::InvalidInput);
                }
                super::credentials::update_current_cred(|cred| {
                    cred.clear_cap_ambient();
                    Ok(())
                })?;
                return Ok(0);
            }
            if arg3 > CAP_LAST_CAP as usize || arg4 != 0 || arg5 != 0 {
                return Err(KError::InvalidInput);
            }
            match subcommand {
                PR_CAP_AMBIENT_IS_SET => {
                    let held = kprocess::current_cred().has_ambient_cap(arg3 as u32);
                    return Ok(isize::from(held));
                }
                PR_CAP_AMBIENT_RAISE => {
                    super::credentials::update_current_cred(|cred| {
                        cred.raise_cap_ambient(arg3 as u32)
                    })?;
                }
                PR_CAP_AMBIENT_LOWER => {
                    super::credentials::update_current_cred(|cred| {
                        cred.lower_cap_ambient(arg3 as u32)
                    })?;
                }
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
