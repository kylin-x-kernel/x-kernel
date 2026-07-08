// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Thread identity and architecture-specific control syscalls.

#[cfg(not(target_arch = "x86_64"))]
use kerrno::KResult;
#[cfg(target_arch = "x86_64")]
use kerrno::{KError, KResult};

/// ARCH_PRCTL codes
///
/// It is only avaliable on x86_64, and is not convenient
/// to generate automatically via c_to_rust binding.
#[cfg(target_arch = "x86_64")]
#[derive(Debug, Eq, PartialEq, num_enum::TryFromPrimitive)]
#[repr(i32)]
enum ArchPrctlCode {
    /// Set the GS segment base
    SetGs    = 0x1001,
    /// Set the FS segment base
    SetFs    = 0x1002,
    /// Get the FS segment base
    GetFs    = 0x1003,
    /// Get the GS segment base
    GetGs    = 0x1004,
    /// The setting of the flag manipulated by ARCH_SET_CPUID
    GetCpuid = 0x1011,
    /// Enable (addr != 0) or disable (addr == 0) the cpuid instruction for the
    /// calling thread.
    SetCpuid = 0x1012,
}

/// Architecture-specific operations (x86_64 only)
/// Supports FS/GS segment base operations and CPUID control
#[cfg(target_arch = "x86_64")]
pub fn sys_arch_prctl(
    uctx: &mut khal::uspace::UserContext,
    code: i32,
    addr: usize,
) -> KResult<isize> {
    use osvm::VirtMutPtr;

    let code = ArchPrctlCode::try_from(code).map_err(|_| KError::InvalidInput)?;
    debug!("sys_arch_prctl: code = {code:?}, addr = {addr:#x}");

    match code {
        // According to Linux implementation, SetFs & SetGs does not return
        // error at all
        ArchPrctlCode::GetFs => {
            (addr as *mut usize).write_vm(uctx.tls())?;
            Ok(0)
        }
        ArchPrctlCode::SetFs => {
            uctx.set_tls(addr);
            Ok(0)
        }
        ArchPrctlCode::GetGs => {
            (addr as *mut usize).write_vm(uctx.gs_base as _)?;
            Ok(0)
        }
        ArchPrctlCode::SetGs => {
            uctx.gs_base = addr as _;
            Ok(0)
        }
        ArchPrctlCode::GetCpuid => Ok(0),
        ArchPrctlCode::SetCpuid => Err(kerrno::KError::NoSuchDevice),
    }
}

/// Returns the thread ID of the current thread.
pub fn sys_gettid() -> KResult<isize> {
    Ok(kprocess::current_user_tid() as _)
}

/// Sets the `clear_child_tid` pointer for the current thread.
pub fn sys_set_tid_address(clear_child_tid: usize) -> KResult<isize> {
    let current_thread = kprocess::current_user_thread();
    current_thread.set_clear_child_tid(clear_child_tid);
    Ok(kprocess::current_user_tid() as isize)
}
