// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Trap handling.

pub use linkme::{
    distributed_slice as def_trap_handler, distributed_slice as register_trap_handler,
};
use memaddr::VirtAddr;
pub use page_table::PagingFlags as PageFaultFlags;

pub use crate::TrapFrame;

/// A slice of IRQ handler functions.
#[def_trap_handler]
pub static IRQ: [fn(usize) -> bool];

/// A slice of page fault handler functions.
#[def_trap_handler]
pub static PAGE_FAULT: [fn(VirtAddr, PageFaultFlags) -> bool];

#[allow(unused_macros)]
macro_rules! dispatch_irq_trap {
    ($trap:ident, $($args:tt)*) => {{
        let mut iter = $crate::excp::$trap.iter();
        if let Some(func) = iter.next() {
            if iter.next().is_some() {
                warn!("Multiple handlers for trap {} are not currently supported", stringify!($trap));
            }
            func($($args)*)
        } else {
            warn!("No registered handler for trap {}", stringify!($trap));
            false
        }
    }}
}

#[cfg(feature = "kcpu_test")]
pub mod tests_excp {
    use unittest::{
        test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
    };

    use super::*;

    test_fn! {
        using TestResult;

        fn test_irq_slice_empty_by_default() {
            let count = IRQ.iter().count();
            assert_eq!(IRQ.len(), count);
        }
    }

    test_fn! {
        using TestResult;

        fn test_page_fault_slice_empty_by_default() {
            let count = PAGE_FAULT.iter().count();
            assert_eq!(PAGE_FAULT.len(), count);
        }
    }

    test_fn! {
        using TestResult;

        fn test_page_fault_flags_bits() {
            let flags = PageFaultFlags::empty();
            assert!(flags.is_empty());
        }
    }

    tests_name! {
        TEST_EXCP;
        test_irq_slice_empty_by_default,
        test_page_fault_slice_empty_by_default,
        test_page_fault_flags_bits,
    }
}
