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
    asm::{user_atomic_cmpxchg_u32, user_copy},
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

/// Atomically loads a 32-bit word from user memory.
///
/// Implemented as a compare-exchange of zero with itself so no store occurs when
/// the current value is non-zero.
pub fn atomic_load_u32(addr: usize) -> MemResult<u32> {
    let (_, observed) = atomic_cmpxchg_u32(addr, 0, 0)?;
    Ok(observed)
}

/// Atomically tests whether `*addr == expected` without changing the word.
pub fn atomic_u32_eq(addr: usize, expected: u32) -> MemResult<bool> {
    let (exchanged, _) = atomic_cmpxchg_u32(addr, expected, expected)?;
    Ok(exchanged)
}

/// Atomically compare-exchanges a 32-bit word in user memory.
///
/// Returns `Ok((true, old))` when `*addr == old` and the store of `new`
/// succeeded. Returns `Ok((false, observed))` when the word differed from
/// `old` (no store). Returns `Err` on misalignment, out-of-range address, or
/// page fault that could not be resolved inside the user-access window.
///
/// This is the primitive futex / robust-list paths need for race-free updates
/// of userspace futex words.
pub fn atomic_cmpxchg_u32(addr: usize, old: u32, new: u32) -> MemResult<(bool, u32)> {
    if !addr.is_multiple_of(core::mem::align_of::<u32>()) {
        return Err(MemError::InvalidAddr);
    }
    check_access(addr, core::mem::size_of::<u32>())?;

    // Match `VirtMemIo`: keep IRQs masked for the exclusive/atomic sequence so
    // a local interrupt cannot clear the monitor between load and store.
    let _irq = IrqSave::new();
    let mut observed = 0u32;
    let failed = access_user_memory(|| {
        // SAFETY: `check_access` validated the 4-byte user range, `addr` is
        // 4-byte aligned, and `observed` is a live kernel stack slot. The
        // call runs inside `access_user_memory`, so page faults are handled
        // by the exception-table fixup on `user_atomic_cmpxchg_u32`.
        unsafe {
            user_atomic_cmpxchg_u32(
                addr as *mut u32,
                old,
                new,
                core::ptr::addr_of_mut!(observed),
            )
        }
    });
    if unlikely(failed != 0) {
        Err(MemError::NoAccess)
    } else {
        Ok((observed == old, observed))
    }
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
    use unittest_support::TestUserValue;

    use super::{
        USER_SPACE_BASE, USER_SPACE_SIZE, atomic_cmpxchg_u32, atomic_load_u32, atomic_u32_eq,
        check_access, fault_outcome_to_trap_result,
    };

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
    fn test_atomic_cmpxchg_u32_rejects_misaligned() {
        let res = atomic_cmpxchg_u32(USER_SPACE_BASE + 1, 0, 1);
        assert!(matches!(res, Err(MemError::InvalidAddr)));
    }

    #[def_test]
    fn test_atomic_cmpxchg_u32_rejects_out_of_range() {
        let res = atomic_cmpxchg_u32(USER_SPACE_BASE - 4, 0, 1);
        assert!(matches!(res, Err(MemError::NoAccess)));
    }

    #[def_test(user)]
    fn test_atomic_cmpxchg_u32_match_updates_user_word() {
        let word = TestUserValue::<u32>::from_value(10).unwrap();
        let addr = word.as_user_ptr() as usize;

        let res = atomic_cmpxchg_u32(addr, 10, 20).unwrap();
        assert_eq!(res, (true, 10));
        assert_eq!(word.read(), 20);
    }

    #[def_test(user)]
    fn test_atomic_cmpxchg_u32_mismatch_leaves_user_word() {
        let word = TestUserValue::<u32>::from_value(10).unwrap();
        let addr = word.as_user_ptr() as usize;

        let res = atomic_cmpxchg_u32(addr, 5, 20).unwrap();
        assert_eq!(res, (false, 10));
        assert_eq!(word.read(), 10);
    }

    #[def_test(user)]
    fn test_atomic_load_u32_reads_user_word() {
        let word = TestUserValue::<u32>::from_value(42).unwrap();
        let addr = word.as_user_ptr() as usize;

        assert_eq!(atomic_load_u32(addr).unwrap(), 42);
    }

    #[def_test(user)]
    fn test_atomic_u32_eq_matches_user_word() {
        let word = TestUserValue::<u32>::from_value(7).unwrap();
        let addr = word.as_user_ptr() as usize;

        assert!(atomic_u32_eq(addr, 7).unwrap());
        assert!(!atomic_u32_eq(addr, 8).unwrap());
    }

    #[def_test(user)]
    fn test_atomic_cmpxchg_u32_faults_on_unmapped_user_addr() {
        let unmapped = USER_SPACE_BASE + 0x1000;
        let res = atomic_cmpxchg_u32(unmapped, 0, 1);
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
