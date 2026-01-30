//! Thread and process identification syscalls.
//!
//! This module implements thread and process identification operations including:
//! - Process ID queries (getpid, getppid, etc.)
//! - Thread ID queries (gettid, etc.)
//! - Architecture-specific controls (arch_prctl, etc.)

use kcore::task::AsThread;
use kerrno::{KError, KResult};
use ktask::current;

/// Get the process ID of the current process
pub fn sys_getpid() -> KResult<isize> {
    Ok(current().as_thread().proc_data.proc.pid() as _)
}

/// Get the parent process ID of the current process
pub fn sys_getppid() -> KResult<isize> {
    current()
        .as_thread()
        .proc_data
        .proc
        .parent()
        .ok_or(KError::NoSuchProcess)
        .map(|p| p.pid() as _)
}

/// Get the thread ID of the current thread
pub fn sys_gettid() -> KResult<isize> {
    Ok(current().id().as_u64() as _)
}

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

/// To set the clear_child_tid field in the task extended data.
///
/// The set_tid_address() always succeeds
/// Set the thread ID address for thread termination notification
/// Always succeeds and returns the current thread ID
pub fn sys_set_tid_address(clear_child_tid: usize) -> KResult<isize> {
    let curr = current();
    curr.as_thread().set_clear_child_tid(clear_child_tid);
    Ok(curr.id().as_u64() as isize)
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
