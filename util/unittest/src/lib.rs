// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]

extern crate self as unittest;

#[macro_use]
extern crate log;
extern crate alloc;

mod arch;
pub mod runner;
pub mod test_framework;
mod user_stack;

// Re-export the def_test and mod_test macros from unittest-macros crate
pub use macros::{def_test, mod_test};
// Re-export the test runner function
pub use runner::{test_run, test_run_filtered, test_run_ok, test_run_ok_filtered};
// Re-export hidden helper functions for assertion macros
// These are used internally by the assertion macros and should not be called directly
#[doc(hidden)]
pub use test_framework::{__log_assert_eq_failure, __log_assert_failure, __log_assert_ne_failure};
// Re-export commonly used types
pub use test_framework::{
    TestDescriptor, TestExecutionMode, TestRunner, TestStats, Testable,
    register_custom_test_executor, register_user_test_executor,
};
pub use user_stack::run_test_on_user_stack;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum TestResult {
    Ok,
    Failed,
    Ignored,
}
