// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Tee Unit Test Framework
//!
//! This module implements a custom unit test framework for Rust code.
//! The framework supports manual test case registration and provides basic assertion functionality.

use alloc::{collections::BTreeMap, vec::Vec};
use core::{
    cell::UnsafeCell,
    fmt::{Arguments, Write},
    hint::spin_loop,
    sync::atomic::{AtomicBool, AtomicUsize, Ordering},
};

use super::TestResult;

/// Function used to print a formatted unit-test message.
///
/// The callback must consume `args` synchronously. It must not call back into
/// `ktest_println!`, which would recurse through the registered printer.
pub type UnittestPrintFn = for<'a> fn(Arguments<'a>);

static UNITTEST_PRINTER: AtomicUsize = AtomicUsize::new(0);

#[doc(hidden)]
pub fn __print_unittest(args: Arguments<'_>) {
    if let Some(printer_fn) = unittest_printer() {
        printer_fn(args);
    }
}

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

pub type UserTestExecutor = fn(&TestDescriptor) -> TestResult;

struct TestExecutorSlot<T: Copy> {
    locked: AtomicBool,
    value: UnsafeCell<Option<T>>,
}

impl<T: Copy> TestExecutorSlot<T> {
    const fn new() -> Self {
        Self {
            locked: AtomicBool::new(false),
            value: UnsafeCell::new(None),
        }
    }

    fn store(&self, executor: T, _ordering: Ordering) {
        self.with_lock(|slot| *slot = Some(executor));
    }

    fn load(&self, _ordering: Ordering) -> Option<T> {
        self.with_lock(|slot| *slot)
    }

    fn with_lock<R>(&self, f: impl FnOnce(&mut Option<T>) -> R) -> R {
        while self
            .locked
            .compare_exchange(false, true, Ordering::Acquire, Ordering::Relaxed)
            .is_err()
        {
            spin_loop();
        }
        // SAFETY: `locked` is this slot's private spinlock. Holding it gives
        // exclusive access to the `UnsafeCell<Option<T>>` contents until the
        // flag is cleared below.
        let result = f(unsafe { &mut *self.value.get() });
        self.locked.store(false, Ordering::Release);
        result
    }
}

// SAFETY: all access to `value` is serialized by `locked`, and `T: Copy`
// prevents borrow-based aliasing from escaping the slot.
unsafe impl<T: Copy> Sync for TestExecutorSlot<T> {}

static USER_TEST_EXECUTOR: TestExecutorSlot<UserTestExecutor> = TestExecutorSlot::new();

/// Registers the callback used for all unittest framework output.
///
/// The callback may be invoked concurrently by parallel test workers. Calls
/// made before a printer is registered produce no output, so callers should
/// register it before starting tests.
pub fn set_printer(printer_fn: UnittestPrintFn) {
    UNITTEST_PRINTER.store(printer_fn as usize, Ordering::Release);
}

fn unittest_printer() -> Option<UnittestPrintFn> {
    let printer_addr = UNITTEST_PRINTER.load(Ordering::Acquire);
    if printer_addr == 0 {
        return None;
    }

    // SAFETY: `UNITTEST_PRINTER` is private and is only written by
    // `set_printer`, which stores a valid `UnittestPrintFn` address. Zero is
    // handled above, and all supported targets represent function pointers in
    // one `usize`.
    Some(unsafe { core::mem::transmute::<usize, UnittestPrintFn>(printer_addr) })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum TestExecutionMode {
    Standard = 0,
    User     = 1,
}

pub fn register_user_test_executor(executor: UserTestExecutor) {
    USER_TEST_EXECUTOR.store(executor, Ordering::Release);
}

fn user_test_executor() -> Option<UserTestExecutor> {
    USER_TEST_EXECUTOR.load(Ordering::Acquire)
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
    pub magic: u64,
    pub name: &'static str,
    pub module: &'static str,
    pub test_fn: fn() -> TestResult,
    pub should_panic: bool,
    pub ignore: bool,
    pub serial: bool,
    pub execution_mode: TestExecutionMode,
}

impl TestDescriptor {
    /// Marker for descriptors emitted by `#[def_test]`, encoded as the ASCII
    /// bytes `XUNITTES` in big-endian hex form.
    pub const MAGIC: u64 = 0x5855_4e49_5454_4553;

    pub const fn new(
        name: &'static str,
        module: &'static str,
        test_fn: fn() -> TestResult,
        should_panic: bool,
        ignore: bool,
        serial: bool,
        execution_mode: TestExecutionMode,
    ) -> Self {
        Self {
            magic: Self::MAGIC,
            name,
            module,
            test_fn,
            should_panic,
            ignore,
            serial,
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
            TestExecutionMode::User => user_test_executor().map_or_else(
                || {
                    crate::ktest_println!(
                        "test {}::{} failed: user test executor is not registered",
                        self.module,
                        self.name
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

    fn write_best_effort(&mut self, args: Arguments<'_>) {
        // A reporting failure must not change test execution or recorded results.
        let _ = self.write_fmt(args);
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
        self.output.write_best_effort(format_args!(
            "  Running test: {}:{}",
            test.module(),
            test.name()
        ));
        crate::ktest_println!("{}", self.output.as_str());

        // Run the test
        let result = test.run();

        // Print test result
        self.output.clear();
        match result {
            TestResult::Ok => {
                self.output.write_best_effort(format_args!(
                    "  test {}::{} ... ok",
                    test.module(),
                    test.name()
                ));
            }
            TestResult::Failed => {
                self.output.write_best_effort(format_args!(
                    "  test {}::{} failed: test function returned TestResult::Failed",
                    test.module(),
                    test.name()
                ));
            }
            TestResult::Ignored => {
                self.output.write_best_effort(format_args!(
                    "  test {}::{} ... ignored",
                    test.module(),
                    test.name()
                ));
            }
        }
        crate::ktest_println!("{}", self.output.as_str());

        // Update statistics
        self.stats.add_result(result);

        result
    }

    pub fn run_tests_descriptors(&mut self, name: &str, tests: &[TestDescriptor]) {
        self.stats = TestStats::new();

        crate::ktest_println!("--------------------------------");
        crate::ktest_println!("Starting unit tests [{}]...", name);

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

    /// Run tests grouped by module.
    /// Passing tests run silently; only failures are printed before the final summary.
    pub fn run_tests_grouped(
        &mut self,
        _name: &str,
        grouped: &BTreeMap<&'static str, Vec<&TestDescriptor>>,
    ) {
        self.stats = TestStats::new();
        let mut saw_failure = false;

        for (module, tests) in grouped {
            let mut module_failures = Vec::new();
            for test in tests {
                let result = test.run();
                self.stats.add_result(result);
                if !result.is_ok() {
                    module_failures.push((test, result));
                }
            }

            if module_failures.is_empty() {
                continue;
            }

            saw_failure = true;
            crate::ktest_println!();
            crate::ktest_println!("  [{}] ({} tests)", module, tests.len());
            crate::ktest_println!("  --------------------------------");
            for (test, result) in module_failures {
                self.print_failed_test(test, result);
            }
        }

        if saw_failure {
            crate::ktest_println!();
        }
        self.print_final_stats();

        if self.stats.failed > 0 {
            TEST_FAILED_FLAG.store(true, Ordering::Relaxed);
        }
    }

    fn print_failed_test(&mut self, test: &TestDescriptor, result: TestResult) {
        self.output.clear();
        self.output
            .write_best_effort(format_args!("    {}", test.name()));
        crate::ktest_println!("{}", self.output.as_str());

        self.output.clear();
        match result {
            TestResult::Failed => {
                self.output
                    .write_best_effort(format_args!("      => FAILED"));
                crate::ktest_println!("{}", self.output.as_str());
            }
            TestResult::Ignored => {
                self.output
                    .write_best_effort(format_args!("      => IGNORED"));
                crate::ktest_println!("{}", self.output.as_str());
            }
            TestResult::Ok => {}
        }
    }

    /// Run a single test without printing module info (for grouped output)
    #[cfg(unittest)]
    fn run_test_simple(&mut self, test: &TestDescriptor) -> TestResult {
        self.output.clear();

        // Print test name only
        self.output
            .write_best_effort(format_args!("    {}", test.name()));
        crate::ktest_println!("{}", self.output.as_str());

        // Run the test
        let result = test.run();

        // Print test result
        self.output.clear();
        match result {
            TestResult::Ok => {
                self.output
                    .write_best_effort(format_args!("      {} ... ok", test.name()));
            }
            TestResult::Failed => {
                self.output.write_best_effort(format_args!(
                    "      {}::{} failed: test function returned TestResult::Failed",
                    test.module(),
                    test.name()
                ));
            }
            TestResult::Ignored => {
                self.output
                    .write_best_effort(format_args!("      {} ... ignored", test.name()));
            }
        }
        crate::ktest_println!("{}", self.output.as_str());

        // Update statistics
        self.stats.add_result(result);

        result
    }

    pub fn print_final_stats(&mut self) {
        self.output.clear();
        self.output.write_best_effort(format_args!(
            "  >>> Test results: {} passed, {} failed, {} ignored, {} total",
            self.stats.passed, self.stats.failed, self.stats.ignored, self.stats.total
        ));
        crate::ktest_println!("{}", self.output.as_str());
        crate::ktest_println!(
            "  >>> Unit tests {}",
            if self.stats.failed > 0 {
                "FAILED"
            } else {
                "PASSED"
            }
        );
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

// Basic assertion macros
#[macro_export]
macro_rules! assert_eq {
    ($left:expr, $right:expr) => {{
        let left_val = &$left;
        let right_val = &$right;
        if left_val != right_val {
            $crate::ktest_println!(
                "assert_eq! failed at {}:{}: {} ({:x?}) == {} ({:x?})",
                file!(),
                line!(),
                stringify!($left),
                left_val,
                stringify!($right),
                right_val
            );
            return $crate::TestResult::Failed;
        }
    }};
    // Tolerate a trailing comma with no message (`assert_eq!(a, b,)`).
    ($left:expr, $right:expr,) => {
        $crate::assert_eq!($left, $right)
    };
    ($left:expr, $right:expr, $($arg:tt)+) => {{
        let left_val = &$left;
        let right_val = &$right;
        if left_val != right_val {
            $crate::ktest_println!(
                "assert_eq! failed at {}:{}: {} ({:x?}) == {} ({:x?}): {}",
                file!(),
                line!(),
                stringify!($left),
                left_val,
                stringify!($right),
                right_val,
                format_args!($($arg)+)
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
            $crate::ktest_println!(
                "assert_ne! failed at {}:{}: {} ({:x?}) != {} ({:x?})",
                file!(),
                line!(),
                stringify!($left),
                left_val,
                stringify!($right),
                right_val
            );
            return $crate::TestResult::Failed;
        }
    }};
    // Tolerate a trailing comma with no message (`assert_ne!(a, b,)`).
    ($left:expr, $right:expr,) => {
        $crate::assert_ne!($left, $right)
    };
    ($left:expr, $right:expr, $($arg:tt)+) => {{
        let left_val = &$left;
        let right_val = &$right;
        if left_val == right_val {
            $crate::ktest_println!(
                "assert_ne! failed at {}:{}: {} ({:x?}) != {} ({:x?}): {}",
                file!(),
                line!(),
                stringify!($left),
                left_val,
                stringify!($right),
                right_val,
                format_args!($($arg)+)
            );
            return $crate::TestResult::Failed;
        }
    }};
}

#[macro_export]
macro_rules! assert {
    ($cond:expr) => {
        if !$cond {
            $crate::ktest_println!(
                "assert! failed at {}:{}: {}",
                file!(),
                line!(),
                stringify!($cond)
            );
            return $crate::TestResult::Failed;
        }
    };
    // Tolerate a trailing comma with no message (`assert!(cond,)`).
    ($cond:expr,) => {
        $crate::assert!($cond)
    };
    ($cond:expr, $($arg:tt)+) => {
        if !$cond {
            $crate::ktest_println!(
                "assert! failed at {}:{}: {}: {}",
                file!(),
                line!(),
                stringify!($cond),
                format_args!($($arg)+)
            );
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
                    false, // serial
                    $crate::TestExecutionMode::Standard,
                ),
            )*
        ];
    };
}

#[macro_export]
macro_rules! tests_name {
    ($suite_name:ident; $module_name:ident; $($test_name:ident),* $(,)?) => {
        #[expect(clippy::allow_attributes, reason = "CG-024-lint-exceptions: the exported macro publishes a suite for caller-selected registration paths; reachability depends on the expansion module")]
        #[allow(
            unreachable_pub,
            reason = "CG-027-narrow-visibility: the exported macro publishes a suite for caller-selected registration paths; reachability depends on the expansion module"
        )]
        pub static $suite_name: &[$crate::TestDescriptor] = &[
            $(
                $crate::TestDescriptor::new(
                    stringify!($test_name),
                    stringify!($module_name),
                    $test_name,
                    false, // should_panic
                    false, // ignore
                    false, // serial
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

#[path = "../ut-tests/framework.rs"]
mod framework_tests;
