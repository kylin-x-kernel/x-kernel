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
        // SAFETY: `_ex_table_start` and `_ex_table_end` are linker-defined
        // symbols bounding the `__ex_table` section. They form a valid slice
        // of `ExceptionTableEntry` items. The table has been sorted by
        // `init_exception_table()` at boot.
        let entries = unsafe {
            core::slice::from_raw_parts(
                _ex_table_start.as_ptr(),
                _ex_table_end
                    .as_ptr()
                    .offset_from_unsigned(_ex_table_start.as_ptr()),
            )
        };
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
    // SAFETY: `_ex_table_start` / `_ex_table_end` are valid linker symbols.
    // This is called once during boot (single-threaded), so mutable access
    // is exclusive. `cast_mut()` is safe because we own the section data
    // at init time.
    let ex_table = unsafe {
        core::slice::from_raw_parts_mut(
            _ex_table_start.as_ptr().cast_mut(),
            _ex_table_end
                .as_ptr()
                .offset_from_unsigned(_ex_table_start.as_ptr()),
        )
    };
    ex_table.sort_unstable();
}

#[cfg(unittest)]
pub mod tests_userspace_common {
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
