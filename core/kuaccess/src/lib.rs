// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Kernel-side user memory access glue for `osvm`.

#![no_std]
#![feature(likely_unlikely)]
#![warn(missing_docs)]

extern crate alloc;

use alloc::string::String;
use core::{ffi::c_char, hint::unlikely, mem::MaybeUninit};

use extern_trait::extern_trait;
use kaddr_layout::{USER_SPACE_BASE, USER_SPACE_SIZE};
use kerrno::{KError, KResult};
use khal::{
    asm::user_copy,
    paging::MappingFlags,
    trap::{PAGE_FAULT, register_trap_handler},
};
use kprocess::AsThread;
use kspin::IrqSave;
use ktask::{current, current_may_uninit};
use memaddr::VirtAddr;
use memspace::PageFaultOutcome;
use osvm::{MemError, MemResult, VirtMemIo};

/// Enables scoped access into user memory, allowing page faults to occur inside
/// kernel.
pub fn access_user_memory<R>(f: impl FnOnce() -> R) -> R {
    let curr = current();
    let Some(thread) = curr.try_as_thread() else {
        panic!("access_user_memory called outside of thread context");
    };

    thread.set_accessing_user_memory(true);
    let result = f();
    thread.set_accessing_user_memory(false);
    result
}

#[register_trap_handler(PAGE_FAULT)]
fn dispatch_irq_page_fault(vaddr: VirtAddr, access_flags: MappingFlags) -> bool {
    let Some(curr) = current_may_uninit() else {
        return false;
    };
    let Some(thread) = curr.try_as_thread() else {
        return false;
    };

    if unlikely(!thread.is_accessing_user_memory()) {
        return false;
    }

    let outcome = thread
        .process()
        .address_space()
        .expect("accessing user memory requires a live process address space")
        .lock()
        .handle_page_fault(vaddr, access_flags);
    fault_outcome_to_trap_result(outcome)
}

fn fault_outcome_to_trap_result(outcome: PageFaultOutcome) -> bool {
    matches!(
        outcome,
        PageFaultOutcome::Resolved | PageFaultOutcome::Retry | PageFaultOutcome::CowConflictRetry
    )
}

// `Vm` mirrors the osvm `VirtMemIo` entry point and is kept for future
// direct VM access paths that instantiate the trait-backed adapter.
#[expect(dead_code)]
struct Vm(IrqSave);

/// Briefly checks if the given memory region is valid user memory.
pub fn check_access(start: usize, len: usize) -> MemResult {
    const USER_SPACE_END: usize = USER_SPACE_BASE + USER_SPACE_SIZE;
    let is_accessible =
        (USER_SPACE_BASE..USER_SPACE_END).contains(&start) && (USER_SPACE_END - start) >= len;
    if unlikely(!is_accessible) {
        Err(MemError::NoAccess)
    } else {
        Ok(())
    }
}

/// Load a null-terminated string from user virtual memory.
pub fn vm_load_string(ptr: *const c_char) -> KResult<String> {
    #[allow(clippy::unnecessary_cast)]
    let bytes = osvm::load_vec_until_null(ptr as *const u8)?;
    String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
}

/// Load a string with a fixed byte length from user virtual memory.
pub fn vm_load_string_with_len(ptr: *const c_char, len: usize) -> KResult<String> {
    #[allow(clippy::unnecessary_cast)]
    let bytes = osvm::load_vec(ptr as *const u8, len)?;
    String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
}

#[extern_trait]
// SAFETY: `Vm` validates the user range up front and performs raw copies only
// inside the temporary user-access window established by `access_user_memory`.
unsafe impl VirtMemIo for Vm {
    fn new() -> Self {
        Self(IrqSave::new())
    }

    fn read_mem(&mut self, start: usize, buf: &mut [MaybeUninit<u8>]) -> MemResult {
        check_access(start, buf.len())?;
        let failed_at = access_user_memory(|| {
            // SAFETY: `check_access` validated the user range, and `buf`
            // provides writable storage for exactly `buf.len()` bytes.
            unsafe { user_copy(buf.as_mut_ptr() as *mut _, start as _, buf.len()) }
        });
        if unlikely(failed_at != 0) {
            Err(MemError::NoAccess)
        } else {
            Ok(())
        }
    }

    fn write_mem(&mut self, start: usize, buf: &[u8]) -> MemResult {
        check_access(start, buf.len())?;
        let failed_at = access_user_memory(|| {
            // SAFETY: `check_access` validated the user range, and `buf`
            // supplies readable storage for exactly `buf.len()` bytes.
            unsafe { user_copy(start as _, buf.as_ptr() as *const _, buf.len()) }
        });
        if unlikely(failed_at != 0) {
            Err(MemError::NoAccess)
        } else {
            Ok(())
        }
    }
}

#[cfg(unittest)]
mod tests {
    use memspace::PageFaultOutcome;
    use osvm::MemError;
    use unittest::def_test;

    use super::{USER_SPACE_BASE, USER_SPACE_SIZE, check_access, fault_outcome_to_trap_result};

    #[def_test]
    fn test_check_access_valid() {
        assert!(check_access(USER_SPACE_BASE, 1).is_ok());
    }

    #[def_test]
    fn test_check_access_invalid_low() {
        let res = check_access(USER_SPACE_BASE - 1, 1);
        assert!(matches!(res, Err(MemError::NoAccess)));
    }

    #[def_test]
    fn test_check_access_invalid_len() {
        let res = check_access(USER_SPACE_BASE, USER_SPACE_SIZE + 1);
        assert!(matches!(res, Err(MemError::NoAccess)));
    }

    #[def_test]
    fn test_check_access_zero_len_and_upper_boundary() {
        assert!(check_access(USER_SPACE_BASE, 0).is_ok());
        assert!(check_access(USER_SPACE_BASE + USER_SPACE_SIZE - 1, 1).is_ok());
        assert!(check_access(USER_SPACE_BASE + USER_SPACE_SIZE, 0).is_err());
    }

    #[def_test]
    fn test_check_access_end_overflow_rejected() {
        let res = check_access(USER_SPACE_BASE + USER_SPACE_SIZE - 1, 2);
        assert!(matches!(res, Err(MemError::NoAccess)));
    }

    #[def_test]
    fn test_check_access_rejects_far_above_user_space() {
        let res = check_access(USER_SPACE_BASE + USER_SPACE_SIZE + 0x1000, 1);
        assert!(matches!(res, Err(MemError::NoAccess)));
    }

    #[def_test]
    fn fault_outcome_mapping_keeps_retry_inside_copy_window() {
        assert!(fault_outcome_to_trap_result(PageFaultOutcome::Resolved));
        assert!(fault_outcome_to_trap_result(PageFaultOutcome::Retry));
        assert!(fault_outcome_to_trap_result(
            PageFaultOutcome::CowConflictRetry
        ));
    }

    #[def_test]
    fn fault_outcome_mapping_rejects_user_copy_failures() {
        assert!(!fault_outcome_to_trap_result(PageFaultOutcome::Unmapped));
        assert!(!fault_outcome_to_trap_result(
            PageFaultOutcome::AccessDenied
        ));
        assert!(!fault_outcome_to_trap_result(PageFaultOutcome::BusError));
        assert!(!fault_outcome_to_trap_result(PageFaultOutcome::OutOfMemory));
        assert!(!fault_outcome_to_trap_result(PageFaultOutcome::NoProgress));
        assert!(!fault_outcome_to_trap_result(PageFaultOutcome::Failed));
    }
}
