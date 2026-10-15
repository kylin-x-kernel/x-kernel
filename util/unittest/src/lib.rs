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

/// Declares a file-backed unittest module.
///
/// This is the file-module counterpart of `mod_test`. Attribute macros on
/// file modules (`mod tests;`) are unstable, so `#[mod_test]` cannot be used
/// together with `#[path = "..."]`. This macro keeps the same `cfg(unittest)`
/// gate and module-level lint exemptions for tests moved out of production
/// source files. Additional module attributes can be passed before `mod`.
///
/// Callers shared with allocator-free boot loaders must also put
/// `#[cfg(unittest)]` on the macro invocation. The gate inside the expansion
/// removes the test module, but resolving this macro still references the
/// `unittest` runtime crate, which depends on `alloc`.
/// This only prevents the reference when `cfg(unittest)` is false; it does not
/// remove an unconditional Cargo dependency from the build graph. XKMake clears
/// kernel unittest flags for boot helpers. Direct Cargo builds must likewise
/// exclude `--cfg unittest`; the outer gate alone cannot isolate such builds.
#[macro_export]
macro_rules! path_mod_test {
    ($(#[$meta:meta])* $vis:vis mod $name:ident, $path:literal $(,)?) => {
        #[cfg(unittest)]
        $(#[$meta])*
        #[path = $path]
        #[allow(
            clippy::unwrap_used,
            clippy::expect_used,
            clippy::panic,
            reason = "CG-021-no-panic-on-reachable-failure: unittest module: test helpers may panic on failure"
        )]
        #[allow(
            missing_docs,
            reason = "CG-024-lint-exceptions: missing_docs: unittest module: test code is outside rustdoc scope"
        )]
        #[allow(
            clippy::too_many_lines,
            reason = "CG-028-focused-functions: unittest module: a test scenario reads as one linear script"
        )]
        $vis mod $name;
    };
    ($name:ident, $path:literal $(,)?) => {
        $crate::path_mod_test!(mod $name, $path);
    };
}

pub mod runner;
#[path = "framework.rs"]
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
