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
use ktask::current;
use kuaccess::vm_load_string;
use linux_raw_sys::general::{__user_cap_data_struct, __user_cap_header_struct};
use osvm::write_vm_mem;
use posix_types::UserPtr;

const CAPABILITY_VERSION_3: u32 = 0x20080522;
const CAP_LAST_CAP: usize = 40;

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

pub fn sys_capget(
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

pub fn sys_capset(
    header: UserPtr<__user_cap_header_struct>,
    _data: UserPtr<__user_cap_data_struct>,
) -> KResult<isize> {
    validate_cap_header(header)?;

    Ok(0)
}

pub fn sys_get_mempolicy(
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
/// - PR_SET_SECCOMP: enable seccomp mode, with the mode specified in `arg2`
/// - PR_CAPBSET_READ: return whether a capability is in the bounding set
/// - PR_MCE_KILL: set the machine check exception policy
/// - PR_SET_MM options: set various memory management options (start/end code/data/brk/stack)
pub fn sys_prctl(
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
        PR_SET_SECCOMP => {}
        PR_CAPBSET_READ => {
            if arg2 > CAP_LAST_CAP {
                return Err(KError::InvalidInput);
            }
            return Ok(1);
        }
        PR_MCE_KILL => {}
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
