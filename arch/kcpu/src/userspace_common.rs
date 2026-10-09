// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Common user-space return reason and exception helpers.

use memaddr::VirtAddr;

use crate::{ExceptionContext, excp::PageFaultFlags, userspace::ExceptionInfo};

/// A reason as to why the control of the CPU is returned from
/// the user space to the kernel.
#[derive(Debug, Clone, Copy)]
pub enum ReturnReason {
    /// An interrupt.
    Interrupt,
    /// A system call.
    Syscall,
    /// A page fault.
    PageFault(VirtAddr, PageFaultFlags),
    /// Other kinds of exceptions.
    Exception(ExceptionInfo),
    /// Unknown reason.
    Unknown,
}

/// A generalized kind for [`ExceptionInfo`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExceptionKind {
    /// A breakpoint exception.
    Breakpoint,
    /// An illegal instruction exception.
    IllegalInstruction,
    /// A misaligned access exception.
    Misaligned,
    /// Other kinds of exceptions.
    Other,
}

#[repr(C)]
#[derive(Debug, PartialEq, Eq, PartialOrd, Ord)]
struct ExceptionTableEntry {
    from: usize,
    to: usize,
}

unsafe extern "C" {
    static _ex_table_start: [ExceptionTableEntry; 0];
    static _ex_table_end: [ExceptionTableEntry; 0];
}

impl ExceptionContext {
    pub(crate) fn fixup_exception(&mut self) -> bool {
        let start = core::ptr::addr_of!(_ex_table_start).cast::<ExceptionTableEntry>();
        let end = core::ptr::addr_of!(_ex_table_end).cast::<ExceptionTableEntry>();
        // SAFETY: The linker places both symbols at the ordered boundaries of
        // one contiguous __ex_table section of ExceptionTableEntry records.
        let len = unsafe { end.offset_from_unsigned(start) };
        // SAFETY: The linker supplies aligned, initialized entries covering len.
        // Boot sorted the table before publication; entries remain immutable.
        let entries = unsafe { core::slice::from_raw_parts(start, len) };
        match entries.binary_search_by(|e| e.from.cmp(&self.ip())) {
            Ok(entry) => {
                self.set_ip(entries[entry].to);
                true
            }
            Err(_) => false,
        }
    }
}

pub(crate) fn init_exception_table() {
    let start = core::ptr::addr_of!(_ex_table_start).cast::<ExceptionTableEntry>();
    let end = core::ptr::addr_of!(_ex_table_end).cast::<ExceptionTableEntry>();
    // SAFETY: The linker places both symbols at the ordered boundaries of
    // one contiguous __ex_table section of ExceptionTableEntry records.
    let len = unsafe { end.offset_from_unsigned(start) };
    // SAFETY: The linker supplies aligned, initialized entries covering len.
    // Single-threaded boot owns this writable section before publication.
    let ex_table = unsafe { core::slice::from_raw_parts_mut(start.cast_mut(), len) };
    ex_table.sort_unstable();
}

#[unittest::mod_test]
pub(crate) mod tests_userspace_common {
    use unittest::def_test;

    use super::{ExceptionKind, ReturnReason};

    #[def_test]
    fn test_exception_kind_equality() {
        assert_ne!(ExceptionKind::Breakpoint, ExceptionKind::Misaligned);
    }

    #[def_test]
    fn test_exception_kind_variants_distinct() {
        assert_ne!(ExceptionKind::IllegalInstruction, ExceptionKind::Other);
        assert_ne!(ExceptionKind::Misaligned, ExceptionKind::Breakpoint);
    }

    #[def_test]
    fn test_return_reason_match() {
        let reason = ReturnReason::Syscall;
        assert!(matches!(reason, ReturnReason::Syscall));
    }
}
