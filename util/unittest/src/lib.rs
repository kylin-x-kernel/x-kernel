// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

extern crate self as unittest;

extern crate alloc;

/// Prints one line through the registered unittest printer.
#[macro_export]
macro_rules! ktest_println {
    () => {
        $crate::__print_unittest(format_args!(""))
    };
    ($($arg:tt)+) => {
        $crate::__print_unittest(format_args!($($arg)+))
    };
}

pub mod runner;
pub mod test_framework;

// Re-export the def_test and mod_test macros from unittest-macros crate
pub use macros::{def_test, mod_test};
// Re-export the test runner function
pub use runner::{collect_tests, test_run, test_run_filtered, test_run_ok, test_run_ok_filtered};
#[doc(hidden)]
pub use test_framework::__print_unittest;
// Re-export commonly used types
pub use test_framework::{
    TestDescriptor, TestExecutionMode, TestRunner, TestStats, Testable, UnittestPrintFn,
    register_user_test_executor, set_printer,
};

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TestResult {
    Ok,
    Failed,
    Ignored,
}
