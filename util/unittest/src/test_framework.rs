// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![allow(dead_code)]

//! Tee Unit Test Framework
//!
//! This module implements a custom unit test framework for Rust code.
//! The framework supports manual test case registration and provides basic assertion functionality.

use alloc::{collections::BTreeMap, format, vec::Vec};
use core::{
    fmt::Write,
    sync::atomic::{AtomicBool, Ordering},
};

use super::TestResult;

impl TestResult {
    pub fn is_ok(&self) -> bool {
        matches!(self, TestResult::Ok)
    }

    pub fn is_failed(&self) -> bool {
        matches!(self, TestResult::Failed)
    }
}

// Test statistics
#[derive(Debug, Clone, Copy)]
pub struct TestStats {
    pub total: usize,
    pub passed: usize,
    pub failed: usize,
    pub ignored: usize,
}

impl TestStats {
    pub const fn new() -> Self {
        Self {
            total: 0,
            passed: 0,
            failed: 0,
            ignored: 0,
        }
    }

    pub fn add_result(&mut self, result: TestResult) {
        self.total += 1;
        match result {
            TestResult::Ok => self.passed += 1,
            TestResult::Failed => self.failed += 1,
            TestResult::Ignored => self.ignored += 1,
        }
    }
}

impl Default for TestStats {
    fn default() -> Self {
        Self::new()
    }
}

pub static TEST_FAILED_FLAG: AtomicBool = AtomicBool::new(false);

pub type CustomTestExecutor = fn(&TestDescriptor) -> TestResult;
pub type UserTestExecutor = fn(&TestDescriptor) -> TestResult;

static CUSTOM_TEST_EXECUTOR: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);
static USER_TEST_EXECUTOR: core::sync::atomic::AtomicUsize =
    core::sync::atomic::AtomicUsize::new(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TestExecutionMode {
    Standard = 0,
    Custom   = 1,
    User     = 2,
}

pub fn register_custom_test_executor(executor: CustomTestExecutor) {
    CUSTOM_TEST_EXECUTOR.store(executor as usize, Ordering::Release);
}

pub fn register_user_test_executor(executor: UserTestExecutor) {
    USER_TEST_EXECUTOR.store(executor as usize, Ordering::Release);
}

fn custom_test_executor() -> Option<CustomTestExecutor> {
    let executor = CUSTOM_TEST_EXECUTOR.load(Ordering::Acquire);
    if executor == 0 {
        None
    } else {
        Some(unsafe { core::mem::transmute::<usize, CustomTestExecutor>(executor) })
    }
}

fn user_test_executor() -> Option<UserTestExecutor> {
    let executor = USER_TEST_EXECUTOR.load(Ordering::Acquire);
    if executor == 0 {
        None
    } else {
        Some(unsafe { core::mem::transmute::<usize, UserTestExecutor>(executor) })
    }
}

// Testable trait
pub trait Testable {
    fn run(&self) -> TestResult;
    fn name(&self) -> &'static str;
    fn should_panic(&self) -> bool {
        false
    }
    fn ignore(&self) -> bool {
        false
    }
}

// Test descriptor structure
#[derive(Clone, Copy)]
#[repr(C)]
pub struct TestDescriptor {
    pub name: &'static str,
    pub module: &'static str,
    pub test_fn: fn() -> TestResult,
    pub should_panic: bool,
    pub ignore: bool,
    pub execution_mode: TestExecutionMode,
}

impl TestDescriptor {
    pub const fn new(
        name: &'static str,
        module: &'static str,
        test_fn: fn() -> TestResult,
        should_panic: bool,
        ignore: bool,
        execution_mode: TestExecutionMode,
    ) -> Self {
        Self {
            name,
            module,
            test_fn,
            should_panic,
            ignore,
            execution_mode,
        }
    }

    pub fn module(&self) -> &'static str {
        self.module
    }
}

impl Testable for TestDescriptor {
    fn run(&self) -> TestResult {
        if self.ignore {
            return TestResult::Ignored;
        }

        match self.execution_mode {
            TestExecutionMode::Standard => (self.test_fn)(),
            TestExecutionMode::Custom => custom_test_executor().map_or_else(
                || {
                    error!(
                        "custom test executor is not registered for {}:{}",
                        self.module, self.name
                    );
                    TestResult::Failed
                },
                |executor| executor(self),
            ),
            TestExecutionMode::User => user_test_executor().map_or_else(
                || {
                    error!(
                        "user test executor is not registered for {}:{}",
                        self.module, self.name
                    );
                    TestResult::Failed
                },
                |executor| executor(self),
            ),
        }
    }

    fn name(&self) -> &'static str {
        self.name
    }

    fn should_panic(&self) -> bool {
        self.should_panic
    }

    fn ignore(&self) -> bool {
        self.ignore
    }
}

// Simple string writer for formatted output
pub struct StringWriter {
    buffer: [u8; 256],
    pos: usize,
}

impl StringWriter {
    pub const fn new() -> Self {
        Self {
            buffer: [0; 256],
            pos: 0,
        }
    }

    pub fn as_str(&self) -> &str {
        core::str::from_utf8(&self.buffer[..self.pos]).unwrap_or("")
    }

    pub fn clear(&mut self) {
        self.pos = 0;
    }
}

impl Write for StringWriter {
    fn write_str(&mut self, s: &str) -> core::fmt::Result {
        let bytes = s.as_bytes();
        let remaining = self.buffer.len() - self.pos;
        let to_copy = core::cmp::min(bytes.len(), remaining);

        if to_copy > 0 {
            self.buffer[self.pos..self.pos + to_copy].copy_from_slice(&bytes[..to_copy]);
            self.pos += to_copy;
        }

        Ok(())
    }
}

impl Default for StringWriter {
    fn default() -> Self {
        Self::new()
    }
}

// Test runner
pub struct TestRunner {
    stats: TestStats,
    output: StringWriter,
}

impl TestRunner {
    pub const fn new() -> Self {
        Self {
            stats: TestStats::new(),
            output: StringWriter::new(),
        }
    }

    pub fn run_test(&mut self, test: &TestDescriptor) -> TestResult {
        self.output.clear();

        // Print test start information with module path
        write!(
            self.output,
            "  Running test: {}:{}",
            test.module(),
            test.name()
        )
        .ok();
        self.print_message(self.output.as_str());

        // Run the test
        let result = test.run();

        // Print test result
        self.output.clear();
        match result {
            TestResult::Ok => {
                write!(self.output, "    Test {} ... OK", test.name()).ok();
            }
            TestResult::Failed => {
                write!(self.output, "    Test {} ... FAILED", test.name()).ok();
            }
            TestResult::Ignored => {
                write!(self.output, "    Test {} ... IGNORED", test.name()).ok();
            }
        }
        self.print_message(self.output.as_str());

        // Update statistics
        self.stats.add_result(result);

        result
    }

    pub fn run_tests_descriptors(&mut self, name: &str, tests: &[TestDescriptor]) {
        self.stats = TestStats::new();

        self.print_message("--------------------------------");
        self.print_message(format!("Starting unit tests [{}]...", name).as_str());

        for test in tests {
            self.run_test(test);
        }

        // Print final statistics
        self.print_final_stats();

        // Set global flag if any test failed
        if self.stats.failed > 0 {
            TEST_FAILED_FLAG.store(true, Ordering::Relaxed);
        }
    }

    /// Run tests grouped by module
    /// Tests from the same module are run together
    pub fn run_tests_grouped(
        &mut self,
        name: &str,
        grouped: &BTreeMap<&'static str, Vec<&TestDescriptor>>,
    ) {
        self.stats = TestStats::new();

        self.print_message("================================");
        self.print_message(format!("Starting unit tests [{}]...", name).as_str());
        self.print_message(format!("  {} module(s) found", grouped.len()).as_str());
        self.print_message("================================");

        for (module, tests) in grouped {
            // Print module header
            self.print_message("");
            self.print_message(format!("  [{}] ({} tests)", module, tests.len()).as_str());
            self.print_message("  --------------------------------");

            // Run all tests in this module
            for test in tests {
                self.run_test_simple(test);
            }
        }

        self.print_message("");
        // Print final statistics
        self.print_final_stats();

        // Set global flag if any test failed
        if self.stats.failed > 0 {
            TEST_FAILED_FLAG.store(true, Ordering::Relaxed);
        }
    }

    /// Run a single test without printing module info (for grouped output)
    fn run_test_simple(&mut self, test: &TestDescriptor) -> TestResult {
        self.output.clear();

        // Print test name only
        write!(self.output, "    {}", test.name()).ok();
        self.print_message(self.output.as_str());

        // Run the test
        let result = test.run();

        // Print test result
        self.output.clear();
        match result {
            TestResult::Ok => {
                write!(self.output, "      => OK").ok();
            }
            TestResult::Failed => {
                write!(self.output, "      => FAILED").ok();
            }
            TestResult::Ignored => {
                write!(self.output, "      => IGNORED").ok();
            }
        }
        self.print_message(self.output.as_str());

        // Update statistics
        self.stats.add_result(result);

        result
    }

    pub fn print_final_stats(&mut self) {
        self.output.clear();
        write!(
            self.output,
            "  >>> Test results: {} passed, {} failed, {} ignored, {} total",
            self.stats.passed, self.stats.failed, self.stats.ignored, self.stats.total
        )
        .ok();
        self.print_message(self.output.as_str());

        if self.stats.failed > 0 {
            self.print_error("  >>> This tests FAILED!");
        } else {
            self.print_message("  >>> This tests PASSED!");
        }
    }

    fn print_message(&self, msg: &str) {
        warn!("{}", msg);
    }

    fn print_error(&self, msg: &str) {
        error!("{}", msg);
    }

    pub fn get_stats(&self) -> TestStats {
        self.stats
    }
}

impl Default for TestRunner {
    fn default() -> Self {
        Self::new()
    }
}

// Helper functions for assertion macros (hidden from docs)
// These allow assertions to work without the caller needing to depend on `log`

#[doc(hidden)]
pub fn __log_assert_eq_failure<T: core::fmt::Debug, U: core::fmt::Debug>(
    file: &str,
    line: u32,
    left_expr: &str,
    left_val: &T,
    right_expr: &str,
    right_val: &U,
) {
    error!(
        "assert_eq! failed at {}:{}: {} ({:x?}) == {} ({:x?})",
        file, line, left_expr, left_val, right_expr, right_val
    );
}

#[doc(hidden)]
pub fn __log_assert_ne_failure<T: core::fmt::Debug, U: core::fmt::Debug>(
    file: &str,
    line: u32,
    left_expr: &str,
    left_val: &T,
    right_expr: &str,
    right_val: &U,
) {
    error!(
        "assert_ne! failed at {}:{}: {} ({:x?}) != {} ({:x?})",
        file, line, left_expr, left_val, right_expr, right_val
    );
}

#[doc(hidden)]
pub fn __log_assert_failure(file: &str, line: u32, cond_expr: &str) {
    error!("assert! failed at {}:{}: {}", file, line, cond_expr);
}

// Basic assertion macros
#[macro_export]
macro_rules! assert_eq {
    ($left:expr, $right:expr) => {{
        let left_val = &$left;
        let right_val = &$right;
        if left_val != right_val {
            $crate::__log_assert_eq_failure(
                file!(),
                line!(),
                stringify!($left),
                left_val,
                stringify!($right),
                right_val,
            );
            return $crate::TestResult::Failed;
        }
    }};
    ($left:expr, $right:expr, $($arg:tt)*) => {{
        let left_val = &$left;
        let right_val = &$right;
        if left_val != right_val {
            $crate::__log_assert_eq_failure(
                file!(),
                line!(),
                stringify!($left),
                left_val,
                stringify!($right),
                right_val,
            );
            return $crate::TestResult::Failed;
        }
    }};
}

#[macro_export]
macro_rules! assert_ne {
    ($left:expr, $right:expr) => {{
        let left_val = &$left;
        let right_val = &$right;
        if left_val == right_val {
            $crate::__log_assert_ne_failure(
                file!(),
                line!(),
                stringify!($left),
                left_val,
                stringify!($right),
                right_val,
            );
            return $crate::TestResult::Failed;
        }
    }};
    ($left:expr, $right:expr, $($arg:tt)*) => {{
        let left_val = &$left;
        let right_val = &$right;
        if left_val == right_val {
            $crate::__log_assert_ne_failure(
                file!(),
                line!(),
                stringify!($left),
                left_val,
                stringify!($right),
                right_val,
            );
            return $crate::TestResult::Failed;
        }
    }};
}

#[macro_export]
macro_rules! assert {
    ($cond:expr) => {
        if !$cond {
            $crate::__log_assert_failure(file!(), line!(), stringify!($cond));
            return $crate::TestResult::Failed;
        }
    };
    ($cond:expr, $($arg:tt)*) => {
        if !$cond {
            $crate::__log_assert_failure(file!(), line!(), stringify!($cond));
            return $crate::TestResult::Failed;
        }
    };
}

// Macros for manually registering test cases
#[macro_export]
macro_rules! tests {
    ($($test_name:ident,)*) => {
        pub static TEST_SUITE: &[$crate::TestDescriptor] = &[
            $(
                $crate::TestDescriptor::new(
                    stringify!($test_name),
                    module_path!(),
                    $test_name,
                    false, // should_panic
                    false, // ignore
                    $crate::TestExecutionMode::Standard,
                ),
            )*
        ];
    };
}

#[macro_export]
macro_rules! tests_name {
    ($suite_name:ident; $module_name:ident; $($test_name:ident),* $(,)?) => {
        pub static $suite_name: &[$crate::TestDescriptor] = &[
            $(
                $crate::TestDescriptor::new(
                    stringify!($test_name),
                    stringify!($module_name),
                    $test_name,
                    false, // should_panic
                    false, // ignore
                    $crate::TestExecutionMode::Standard,
                ),
            )*
        ];
    };
}

#[macro_export]
macro_rules! run_tests {
    // Multiple test suites
    ($runner:expr, [$($tests:expr),+ $(,)?]) => {
        $(
            $runner.run_tests_descriptors(stringify!($tests), $tests);
        )+
    };
    // Single test suite
    ($runner:expr, $test:expr) => {
        $runner.run_tests_descriptors(stringify!($test), $test);
    };
}

pub fn tests_failed() -> bool {
    TEST_FAILED_FLAG.load(Ordering::Relaxed)
}

#[cfg(unittest)]
mod tests_test_framework {
    use alloc::{collections::BTreeMap, vec};
    use core::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use unittest::def_test;

    use super::*;
    use crate::TestResult;

    static CUSTOM_CALLS: AtomicUsize = AtomicUsize::new(0);

    fn ok_test() -> TestResult {
        TestResult::Ok
    }

    fn fail_test() -> TestResult {
        TestResult::Failed
    }

    fn custom_ok_test(_: &TestDescriptor) -> TestResult {
        CUSTOM_CALLS.fetch_add(1, AtomicOrdering::Relaxed);
        TestResult::Ok
    }

    fn assert_ok_test() -> TestResult {
        crate::assert!(true);
        crate::assert_eq!(1_u32, 1_u32);
        crate::assert_ne!(1_u32, 2_u32);
        TestResult::Ok
    }

    fn assert_fail_test() -> TestResult {
        crate::assert_eq!(1_u32, 2_u32);
        TestResult::Ok
    }

    fn ignored_test() -> TestResult {
        TestResult::Ignored
    }

    tests_name!(FRAMEWORK_SUITE; framework; ok_test, assert_ok_test);

    #[def_test]
    fn test_testresult_helpers_and_stats_default() {
        assert!(TestResult::Ok.is_ok());
        assert!(TestResult::Failed.is_failed());

        let stats = TestStats::default();
        assert_eq!(stats.total, 0);
        assert_eq!(stats.passed, 0);
        assert_eq!(stats.failed, 0);
        assert_eq!(stats.ignored, 0);
    }

    #[def_test]
    fn test_string_writer_truncates_and_clear_resets_position() {
        let mut writer = StringWriter::new();
        let long = "x".repeat(300);
        core::fmt::Write::write_str(&mut writer, &long).unwrap();
        assert_eq!(writer.as_str().len(), 256);
        writer.clear();
        assert_eq!(writer.as_str(), "");
    }

    #[def_test]
    fn test_testrunner_run_test_updates_stats() {
        let mut runner = TestRunner::new();
        let desc = TestDescriptor::new(
            "ok_test",
            "framework",
            ok_test,
            false,
            false,
            TestExecutionMode::Standard,
        );

        assert_eq!(runner.run_test(&desc), TestResult::Ok);
        let stats = runner.get_stats();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.passed, 1);
        assert_eq!(stats.failed, 0);
    }

    #[def_test]
    fn test_grouped_runner_sets_failed_flag() {
        TEST_FAILED_FLAG.store(false, Ordering::Relaxed);

        let ok = TestDescriptor::new(
            "ok",
            "alpha",
            ok_test,
            false,
            false,
            TestExecutionMode::Standard,
        );
        let fail = TestDescriptor::new(
            "fail",
            "beta",
            fail_test,
            false,
            false,
            TestExecutionMode::Standard,
        );

        let mut grouped = BTreeMap::new();
        grouped.insert("alpha", vec![&ok]);
        grouped.insert("beta", vec![&fail]);

        let mut runner = TestRunner::new();
        runner.run_tests_grouped("framework", &grouped);

        let stats = runner.get_stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.failed, 1);
        assert!(tests_failed());
        TEST_FAILED_FLAG.store(false, Ordering::Relaxed);
    }

    #[def_test]
    fn test_testdescriptor_ignore_and_name_helpers() {
        let ignored = TestDescriptor::new(
            "ignored",
            "framework",
            fail_test,
            false,
            true,
            TestExecutionMode::Standard,
        );

        assert_eq!(ignored.name(), "ignored");
        assert_eq!(ignored.module(), "framework");
        assert!(!ignored.should_panic());
        assert!(ignored.ignore());
        assert_eq!(ignored.run(), TestResult::Ignored);
    }

    #[def_test]
    fn test_custom_executor_paths() {
        let old = CUSTOM_TEST_EXECUTOR.swap(0, Ordering::AcqRel);
        CUSTOM_CALLS.store(0, AtomicOrdering::Relaxed);

        let desc = TestDescriptor::new(
            "custom",
            "framework",
            fail_test,
            false,
            false,
            TestExecutionMode::Custom,
        );

        assert_eq!(desc.run(), TestResult::Failed);

        register_custom_test_executor(custom_ok_test);
        assert_eq!(desc.run(), TestResult::Ok);
        assert_eq!(CUSTOM_CALLS.load(AtomicOrdering::Relaxed), 1);

        CUSTOM_TEST_EXECUTOR.store(old, Ordering::Release);
    }

    #[def_test]
    fn test_run_tests_descriptors_pass_path_and_tests_failed_false() {
        TEST_FAILED_FLAG.store(false, Ordering::Relaxed);
        let mut runner = TestRunner::new();
        let suite = [TestDescriptor::new(
            "ok_only",
            "framework",
            ok_test,
            false,
            false,
            TestExecutionMode::Standard,
        )];

        runner.run_tests_descriptors("framework", &suite);

        let stats = runner.get_stats();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.passed, 1);
        assert_eq!(stats.failed, 0);
        assert!(!tests_failed());
    }

    #[def_test]
    fn test_run_test_simple_and_default_constructor() {
        let mut runner = TestRunner::default();
        let desc = TestDescriptor::new(
            "assert_ok",
            "framework",
            assert_ok_test,
            false,
            false,
            TestExecutionMode::Standard,
        );

        assert_eq!(runner.run_test_simple(&desc), TestResult::Ok);
        let stats = runner.get_stats();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.passed, 1);
    }

    #[def_test]
    fn test_assertion_macros_failure_path() {
        assert_eq!(assert_fail_test(), TestResult::Failed);
    }

    #[def_test]
    fn test_run_tests_macro_with_named_suite() {
        TEST_FAILED_FLAG.store(false, Ordering::Relaxed);
        let mut runner = TestRunner::new();
        crate::run_tests!(runner, FRAMEWORK_SUITE);

        let stats = runner.get_stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.passed, 2);
        assert_eq!(stats.failed, 0);
        assert!(!tests_failed());
    }

    #[def_test]
    fn test_teststats_add_result_counts_each_variant() {
        let mut stats = TestStats::new();
        stats.add_result(TestResult::Ok);
        stats.add_result(TestResult::Failed);
        stats.add_result(TestResult::Ignored);

        assert_eq!(stats.total, 3);
        assert_eq!(stats.passed, 1);
        assert_eq!(stats.failed, 1);
        assert_eq!(stats.ignored, 1);
    }

    #[def_test]
    fn test_string_writer_multiple_writes_preserve_prefix() {
        let mut writer = StringWriter::new();
        core::fmt::Write::write_str(&mut writer, "hello").unwrap();
        core::fmt::Write::write_str(&mut writer, " world").unwrap();
        assert_eq!(writer.as_str(), "hello world");

        writer.clear();
        core::fmt::Write::write_str(&mut writer, "reset").unwrap();
        assert_eq!(writer.as_str(), "reset");
    }

    #[def_test]
    fn test_descriptor_accessors_and_run_variants() {
        let standard = TestDescriptor::new(
            "standard",
            "framework",
            ok_test,
            true,
            false,
            TestExecutionMode::Standard,
        );
        let ignored = TestDescriptor::new(
            "ignored",
            "framework",
            ignored_test,
            false,
            true,
            TestExecutionMode::Standard,
        );

        assert_eq!(standard.name(), "standard");
        assert_eq!(standard.module(), "framework");
        assert!(standard.should_panic());
        assert!(!standard.ignore());
        assert_eq!(standard.run(), TestResult::Ok);
        assert_eq!(ignored.run(), TestResult::Ignored);
    }

    #[def_test]
    fn test_runner_run_tests_descriptors_failed_sets_global_flag() {
        TEST_FAILED_FLAG.store(false, Ordering::Relaxed);
        let mut runner = TestRunner::new();
        let suite = [
            TestDescriptor::new(
                "ok",
                "framework",
                ok_test,
                false,
                false,
                TestExecutionMode::Standard,
            ),
            TestDescriptor::new(
                "fail",
                "framework",
                fail_test,
                false,
                false,
                TestExecutionMode::Standard,
            ),
        ];

        runner.run_tests_descriptors("framework", &suite);

        let stats = runner.get_stats();
        assert_eq!(stats.total, 2);
        assert_eq!(stats.passed, 1);
        assert_eq!(stats.failed, 1);
        assert!(tests_failed());
        TEST_FAILED_FLAG.store(false, Ordering::Relaxed);
    }

    #[def_test]
    fn test_custom_executor_can_be_restored_after_use() {
        let old = CUSTOM_TEST_EXECUTOR.swap(0, Ordering::AcqRel);
        CUSTOM_CALLS.store(0, AtomicOrdering::Relaxed);

        register_custom_test_executor(custom_ok_test);
        let desc = TestDescriptor::new(
            "custom_restore",
            "framework",
            fail_test,
            false,
            false,
            TestExecutionMode::Custom,
        );

        assert_eq!(desc.run(), TestResult::Ok);
        assert_eq!(CUSTOM_CALLS.load(AtomicOrdering::Relaxed), 1);

        CUSTOM_TEST_EXECUTOR.store(old, Ordering::Release);
    }
}
