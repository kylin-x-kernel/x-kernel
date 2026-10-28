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
    asm::{user_atomic_cmpxchg_u32, user_atomic_load_u32, user_copy},
    paging::MappingFlags,
    trap::{PAGE_FAULT, register_trap_handler},
};
use kprocess::AsThread;
use kspin::IrqSave;
use ktask::{current, current_may_uninit};
use memaddr::{PAGE_SIZE_4K, VirtAddr};
use memspace::PageFaultOutcome;
use osvm::{MemError, MemResult, VirtMemIo};

/// Enables scoped access into user memory, allowing page faults to occur inside
/// kernel.
pub fn access_user_memory<R>(f: impl FnOnce() -> R) -> R {
    let curr = current();
    #[expect(
        clippy::panic,
        reason = "CG-021-no-panic-on-reachable-failure: Fault-resolving user atomics serve futex \
                  and robust-list operations on the user thread that owns the address space; a \
                  kernel task has no user address space to access"
    )]
    let Some(thread) = curr.try_as_thread() else {
        panic!("access_user_memory called outside of thread context");
    };

    thread.set_accessing_user_memory(true);
    let result = f();
    thread.set_accessing_user_memory(false);
    result
}

#[register_trap_handler(PAGE_FAULT)]
fn handle_page_fault(vaddr: VirtAddr, access_flags: MappingFlags) -> bool {
    let Some(curr) = current_may_uninit() else {
        return false;
    };
    let Some(thread) = curr.try_as_thread() else {
        return false;
    };

    if unlikely(!thread.is_accessing_user_memory()) {
        return false;
    }

    let Ok(address_space) = thread.process().address_space() else {
        return false;
    };
    let outcome = address_space.lock().handle_page_fault(vaddr, access_flags);
    fault_outcome_to_trap_result(outcome)
}

fn fault_outcome_to_trap_result(outcome: PageFaultOutcome) -> bool {
    matches!(
        outcome,
        PageFaultOutcome::Resolved | PageFaultOutcome::Retry | PageFaultOutcome::CowConflictRetry
    )
}

// Adapter instantiated by the osvm external-trait entry points.
struct Vm;

// Bound all fault outcomes, including backends that repeatedly report success
// without enabling the access. Leave room for staged faults and transient races.
const MAX_USER_PAGE_FAULT_ATTEMPTS: usize = 16;

// Retry only the current user page after returning through the assembly fixup.
// File-backed faults can wait for disk I/O here, outside exception context.
fn copy_user_pages(
    start: usize,
    len: usize,
    access_flags: MappingFlags,
    mut copy_fn: impl FnMut(usize, usize) -> usize,
) -> MemResult {
    check_access(start, len)?;
    let mut offset = 0;
    let mut fault_attempts = 0;
    while offset < len {
        let address = start + offset;
        let count = (PAGE_SIZE_4K - address % PAGE_SIZE_4K).min(len - offset);
        if copy_fn(offset, count) == 0 {
            offset += count;
            fault_attempts = 0;
            continue;
        }
        if fault_attempts == MAX_USER_PAGE_FAULT_ATTEMPTS {
            return Err(MemError::NoAccess);
        }
        fault_attempts += 1;
        let curr = current();
        let thread = curr.try_as_thread().ok_or(MemError::NoAccess)?;
        let address_space = thread
            .process()
            .address_space()
            .map_err(|_| MemError::NoAccess)?;
        let outcome = address_space
            .lock()
            .handle_page_fault(VirtAddr::from_usize(address), access_flags);
        // Retry the copy after each recoverable fault. Shared file writes may
        // first install a read-only PTE, then fault again to enable writing.
        match outcome {
            PageFaultOutcome::Resolved
            | PageFaultOutcome::Retry
            | PageFaultOutcome::CowConflictRetry => continue,
            _ => return Err(MemError::NoAccess),
        }
    }
    Ok(())
}

unittest::path_mod_test!(copy_tests, "../ut-tests/copy_tests.rs");

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
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: C char is signed on some architectures; user-memory \
                  helpers require byte pointers"
    )]
    #[allow(
        clippy::unnecessary_cast,
        reason = "CG-024-lint-exceptions: clippy::unnecessary_cast: C char is signed on some \
                  architectures; user-memory helpers require byte pointers"
    )]
    let bytes = osvm::load_vec_until_null(ptr as *const u8)?;
    String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
}

/// Load a string with a fixed byte length from user virtual memory.
pub fn vm_load_string_with_len(ptr: *const c_char, len: usize) -> KResult<String> {
    #[expect(
        clippy::allow_attributes,
        reason = "CG-024-lint-exceptions: C char is signed on some architectures; user-memory \
                  helpers require byte pointers"
    )]
    #[allow(
        clippy::unnecessary_cast,
        reason = "CG-024-lint-exceptions: clippy::unnecessary_cast: C char is signed on some \
                  architectures; user-memory helpers require byte pointers"
    )]
    let bytes = osvm::load_vec(ptr as *const u8, len)?;
    String::from_utf8(bytes).map_err(|_| KError::IllegalBytes)
}

/// Atomically loads a 32-bit word from user memory.
pub fn atomic_load_u32(addr: usize) -> MemResult<u32> {
    atomic_load_u32_inner(addr, true)
}

/// Atomically loads a 32-bit user word without resolving page faults.
///
/// This is intended for callers that hold a non-sleepable lock. A fault is
/// reported as [`MemError::NoAccess`]; the caller must drop its lock, fault the
/// address in through [`atomic_load_u32`], and retry the whole operation.
pub fn atomic_load_u32_nofault(addr: usize) -> MemResult<u32> {
    atomic_load_u32_inner(addr, false)
}

/// Atomically tests whether `*addr == expected` without changing the word.
pub fn atomic_u32_eq(addr: usize, expected: u32) -> MemResult<bool> {
    Ok(atomic_load_u32(addr)? == expected)
}

/// Atomically tests a user word without resolving page faults.
///
/// See [`atomic_load_u32_nofault`] for the retry contract.
pub fn atomic_u32_eq_nofault(addr: usize, expected: u32) -> MemResult<bool> {
    Ok(atomic_load_u32_nofault(addr)? == expected)
}

fn atomic_load_u32_inner(addr: usize, resolve_faults: bool) -> MemResult<u32> {
    if !addr.is_multiple_of(core::mem::align_of::<u32>()) {
        return Err(MemError::InvalidAddr);
    }
    check_access(addr, core::mem::size_of::<u32>())?;

    let mut observed = 0u32;
    let mut operation = || {
        // SAFETY: `check_access` validated the 4-byte user range, `addr` is
        // naturally aligned, and `observed` is a live writable kernel stack
        // slot. The assembly exception table converts a user-memory fault into
        // a nonzero return.
        unsafe { user_atomic_load_u32(addr as *const u32, core::ptr::addr_of_mut!(observed)) }
    };
    let failed = if resolve_faults {
        access_user_memory(operation)
    } else {
        operation()
    };
    if unlikely(failed != 0) {
        Err(MemError::NoAccess)
    } else {
        Ok(observed)
    }
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
    atomic_cmpxchg_u32_inner(addr, old, new, true)
}

/// Atomically compare-exchanges a 32-bit user word without fault resolution.
///
/// This operation is safe under a non-sleepable lock because a missing or
/// inaccessible mapping is reported immediately. Callers that can publish the
/// store outside that lock retry through [`atomic_cmpxchg_u32`]. Callers that
/// must keep the store inside the lock, such as `FUTEX_WAKE_OP`, instead call
/// [`fault_in_user_writeable`] and then retry this nofault operation.
pub fn atomic_cmpxchg_u32_nofault(addr: usize, old: u32, new: u32) -> MemResult<(bool, u32)> {
    atomic_cmpxchg_u32_inner(addr, old, new, false)
}

/// Makes one write-fault attempt for a user word without storing a new value.
///
/// This is the slow path for a nofault store that faulted on a present,
/// write-protected page such as a private page after `fork`. One call invokes
/// `MmSpace::handle_page_fault` once. `Resolved` means the address can now be
/// written. A retry-class outcome is also success: another task already
/// published a new PTE or anonymous page, and the caller repeats its store.
/// The caller must not hold a non-sleepable lock.
///
/// # Errors
///
/// Returns [`MemError::InvalidAddr`] when `addr` is not 4-byte aligned,
/// [`MemError::OutOfMemory`] when the fault handler runs out of memory, and
/// [`MemError::NoAccess`] when the address is outside user space or the VMA
/// rejects the write.
pub fn fault_in_user_writeable(addr: usize) -> MemResult<()> {
    if !addr.is_multiple_of(core::mem::align_of::<u32>()) {
        return Err(MemError::InvalidAddr);
    }
    check_access(addr, core::mem::size_of::<u32>())?;

    let curr = current();
    let Some(thread) = curr.try_as_thread() else {
        return Err(MemError::NoAccess);
    };
    let address_space = thread
        .process()
        .address_space()
        .map_err(|_| MemError::NoAccess)?;
    let outcome = address_space
        .lock()
        .handle_page_fault(VirtAddr::from_usize(addr), MappingFlags::WRITE);
    if outcome.is_resolved() || outcome.is_retryable() {
        Ok(())
    } else if outcome == PageFaultOutcome::OutOfMemory {
        Err(MemError::OutOfMemory)
    } else {
        Err(MemError::NoAccess)
    }
}

fn atomic_cmpxchg_u32_inner(
    addr: usize,
    old: u32,
    new: u32,
    resolve_faults: bool,
) -> MemResult<(bool, u32)> {
    if !addr.is_multiple_of(core::mem::align_of::<u32>()) {
        return Err(MemError::InvalidAddr);
    }
    check_access(addr, core::mem::size_of::<u32>())?;

    let mut observed = 0u32;
    let mut operation = || {
        // SAFETY: `check_access` validated the 4-byte user range, `addr` is
        // 4-byte aligned, and `observed` is a live kernel stack slot. The
        // assembly exception table converts a user-memory fault into a nonzero
        // return without exposing a raw user pointer to safe Rust.
        unsafe {
            user_atomic_cmpxchg_u32(
                addr as *mut u32,
                old,
                new,
                core::ptr::addr_of_mut!(observed),
            )
        }
    };
    let failed = if resolve_faults {
        access_user_memory(operation)
    } else {
        // The nofault form is used inside non-sleepable ordering sections.
        // Keep its architecture-exclusive sequence local to this CPU.
        let _irq = IrqSave::new();
        operation()
    };
    if unlikely(failed != 0) {
        Err(MemError::NoAccess)
    } else {
        Ok((observed == old, observed))
    }
}

#[extern_trait]
// SAFETY: `Vm` validates the user range and uses exception-table-protected
// copies. Missing pages are resolved in the calling task before retrying.
unsafe impl VirtMemIo for Vm {
    fn new() -> Self {
        Self
    }

    fn read_mem(&mut self, start: usize, buf: &mut [MaybeUninit<u8>]) -> MemResult {
        copy_user_pages(start, buf.len(), MappingFlags::READ, |offset, count| {
            // SAFETY: the validated user page and this subrange of `buf` cover
            // `count` bytes. The exception table reports inaccessible memory.
            let destination = unsafe { buf.as_mut_ptr().cast::<u8>().add(offset) };
            // SAFETY: copy_user_pages validated the user source; destination
            // covers count exclusively borrowed kernel bytes. Faults are handled
            // by user_copy's exception-table path.
            unsafe { user_copy(destination, (start + offset) as _, count) }
        })
    }

    fn write_mem(&mut self, start: usize, buf: &[u8]) -> MemResult {
        copy_user_pages(start, buf.len(), MappingFlags::WRITE, |offset, count| {
            // SAFETY: the validated user page and this subrange of `buf` cover
            // `count` bytes. The exception table reports inaccessible memory.
            let source = unsafe { buf.as_ptr().add(offset) };
            // SAFETY: The kernel source has count initialized bytes, and
            // copy_user_pages validated the user destination; user_copy handles faults.
            unsafe { user_copy((start + offset) as _, source, count) }
        })
    }
}

unittest::path_mod_test!(tests, "../ut-tests/access_tests.rs");
