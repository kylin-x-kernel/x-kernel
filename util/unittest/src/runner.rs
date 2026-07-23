// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Test collection and runner module
//!
//! This module provides the `test_run()` function that automatically discovers
//! and runs all tests marked with `#[unittest]`.

use alloc::{collections::BTreeMap, vec::Vec};
use core::{
    mem::{self, offset_of},
    sync::atomic::Ordering,
};

use crate::test_framework::{
    TEST_FAILED_FLAG, TestDescriptor, TestExecutionMode, TestRunner, TestStats,
};

// External symbols defined in the linker script
#[allow(improper_ctypes)]
unsafe extern "C" {
    static __unittest_start: u8;
    static __unittest_end: u8;
    static _text: u8;
    static _etext: u8;
    static _srodata: u8;
    static _erodata: u8;
}

/// Get all registered unit tests from the linker section.
///
/// This relies on the linker script defining `__unittest_start` and
/// `__unittest_end` symbols that bracket the `.unittest` section.
fn get_tests() -> Vec<TestDescriptor> {
    // SAFETY: the linker script defines these symbols as the byte bounds of
    // the retained `.unittest` section.
    let start = unsafe { &__unittest_start as *const u8 as usize };
    // SAFETY: see the `__unittest_start` access above.
    let end = unsafe { &__unittest_end as *const u8 as usize };
    let descriptor_size = mem::size_of::<TestDescriptor>();
    let mut tests = Vec::new();
    let mut cursor = start;

    while cursor + descriptor_size <= end {
        if is_valid_descriptor_slot(cursor) {
            // SAFETY: `cursor` points at a non-padding slot in the `.unittest`
            // section. The slot was emitted as a `TestDescriptor` by the
            // `def_test` macro and is static for the kernel lifetime.
            tests.push(unsafe { (cursor as *const TestDescriptor).read_unaligned() });
            cursor += descriptor_size;
        } else {
            cursor += descriptor_size;
        }
    }
    tests
}

fn is_valid_descriptor_slot(addr: usize) -> bool {
    let name_offset = offset_of!(TestDescriptor, name);
    let module_offset = offset_of!(TestDescriptor, module);

    let magic = read_slot_u64(addr + offset_of!(TestDescriptor, magic));
    let name_data = read_slot_word(addr + name_offset);
    let name_len = read_slot_word(addr + name_offset + mem::size_of::<usize>());
    let module_data = read_slot_word(addr + module_offset);
    let module_len = read_slot_word(addr + module_offset + mem::size_of::<usize>());
    let test_fn = read_slot_word(addr + offset_of!(TestDescriptor, test_fn));

    magic == TestDescriptor::MAGIC
        && is_valid_static_str(name_data, name_len)
        && is_valid_static_str(module_data, module_len)
        && is_text_ptr(test_fn)
        && is_valid_bool_byte(read_slot_u8(
            addr + offset_of!(TestDescriptor, should_panic),
        ))
        && is_valid_bool_byte(read_slot_u8(addr + offset_of!(TestDescriptor, ignore)))
        && is_valid_bool_byte(read_slot_u8(addr + offset_of!(TestDescriptor, serial)))
        && is_valid_execution_mode_byte(read_slot_u8(
            addr + offset_of!(TestDescriptor, execution_mode),
        ))
}

fn is_valid_static_str(ptr: usize, len: usize) -> bool {
    const MAX_UNITTEST_STR_LEN: usize = 256;

    len != 0 && len <= MAX_UNITTEST_STR_LEN && range_within(ptr, len, rodata_start(), rodata_end())
}

fn is_text_ptr(ptr: usize) -> bool {
    ptr >= text_start() && ptr < text_end()
}

fn range_within(ptr: usize, len: usize, start: usize, end: usize) -> bool {
    ptr >= start && ptr.checked_add(len).is_some_and(|limit| limit <= end)
}

fn text_start() -> usize {
    // SAFETY: `_text` is a linker-defined section boundary symbol.
    unsafe { &_text as *const u8 as usize }
}

fn text_end() -> usize {
    // SAFETY: `_etext` is a linker-defined section boundary symbol.
    unsafe { &_etext as *const u8 as usize }
}

fn rodata_start() -> usize {
    // SAFETY: `_srodata` is a linker-defined section boundary symbol.
    unsafe { &_srodata as *const u8 as usize }
}

fn rodata_end() -> usize {
    // SAFETY: `_erodata` is a linker-defined section boundary symbol.
    unsafe { &_erodata as *const u8 as usize }
}

fn read_slot_word(addr: usize) -> usize {
    // SAFETY: callers only pass addresses within one descriptor-sized slot
    // bounded by the linker-provided `.unittest` section. `read_unaligned`
    // avoids imposing extra assumptions on linker padding alignment.
    unsafe { (addr as *const usize).read_unaligned() }
}

fn read_slot_u64(addr: usize) -> u64 {
    // SAFETY: callers pass field addresses within one descriptor-sized slot
    // bounded by the linker-provided `.unittest` section.
    unsafe { (addr as *const u64).read_unaligned() }
}

fn read_slot_u8(addr: usize) -> u8 {
    // SAFETY: callers pass field addresses within one descriptor-sized slot
    // bounded by the linker-provided `.unittest` section.
    unsafe { (addr as *const u8).read_unaligned() }
}

fn is_valid_bool_byte(value: u8) -> bool {
    matches!(value, 0 | 1)
}

fn is_valid_execution_mode_byte(value: u8) -> bool {
    value == TestExecutionMode::Standard as u8 || value == TestExecutionMode::User as u8
}

/// Group tests by module path
fn group_tests_by_module<'a>(
    tests: &'a [TestDescriptor],
) -> BTreeMap<&'static str, Vec<&'a TestDescriptor>> {
    let mut grouped: BTreeMap<&'static str, Vec<&'a TestDescriptor>> = BTreeMap::new();

    for test in tests {
        grouped.entry(test.module).or_default().push(test);
    }

    grouped
}

fn normalize_crate_filter(crate_filter: Option<&str>) -> Vec<&str> {
    crate_filter.map_or_else(Vec::new, |raw| {
        raw.split(',')
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .collect()
    })
}

fn module_matches_crate(module: &str, crate_name: &str) -> bool {
    module == crate_name
        || module
            .strip_prefix(crate_name)
            .is_some_and(|rest| rest.starts_with("::"))
}

fn select_tests_by_crate(tests: &[TestDescriptor], crate_filters: &[&str]) -> Vec<TestDescriptor> {
    if crate_filters.is_empty() {
        return tests.to_vec();
    }

    tests
        .iter()
        .copied()
        .filter(|test| {
            crate_filters
                .iter()
                .any(|crate_name| module_matches_crate(test.module, crate_name))
        })
        .collect()
}

/// Run all registered unit tests
///
/// This function discovers all tests marked with `#[unittest]` and runs them.
/// Tests are grouped by module and run together.
/// It prints test results and statistics through `ktest_println!`.
///
/// # Returns
/// `TestStats` containing the results of all tests
///
/// # Example
/// ```rust,no_run
/// unittest::test_run();
/// ```
pub fn test_run() -> TestStats {
    test_run_filtered(None)
}

/// Run unit tests with optional crate filter.
///
/// `crate_filter` supports a single crate name or multiple crate names
/// separated by commas, for example: `"tee_kernel,kvfs"`.
pub fn test_run_filtered(crate_filter: Option<&str>) -> TestStats {
    // Reset the failed flag
    TEST_FAILED_FLAG.store(false, Ordering::Relaxed);

    let mut runner = TestRunner::new();

    // Get tests from linker section
    let tests = get_tests();

    if tests.is_empty() {
        crate::ktest_println!("================================");
        crate::ktest_println!("No tests found!");
        crate::ktest_println!("================================");
        return TestStats::new();
    }

    let crate_filters = normalize_crate_filter(crate_filter);
    let selected_tests = select_tests_by_crate(&tests, &crate_filters);

    if selected_tests.is_empty() {
        let print_no_tests = |msg: &str| crate::ktest_println!("{}", msg);

        print_no_tests("================================");
        if crate_filters.is_empty() {
            print_no_tests("No tests found!");
        } else {
            crate::ktest_println!(
                "No tests found for crate filter: {}",
                crate_filters.join(",")
            );
        }
        print_no_tests("================================");
        return TestStats::new();
    }

    // Group tests by module and run them
    let grouped = group_tests_by_module(&selected_tests);
    runner.run_tests_grouped("unittest", &grouped);

    runner.get_stats()
}

/// Run all tests and return whether all tests passed
pub fn test_run_ok() -> bool {
    test_run_ok_filtered(None)
}

/// Run tests with optional crate filter and return whether all tests passed.
///
/// When filter is provided but no test matches, this returns `false`.
pub fn test_run_ok_filtered(crate_filter: Option<&str>) -> bool {
    let has_filter = !normalize_crate_filter(crate_filter).is_empty();
    let stats = test_run_filtered(crate_filter);
    if has_filter && stats.total == 0 {
        return false;
    }
    stats.failed == 0
}

/// Collect and group tests without running them.
///
/// Returns tests grouped by module path, filtered by the optional crate filter.
/// This is intended for external runners (e.g. entry.rs) that want to control
/// test execution themselves (e.g. parallel scheduling).
pub fn collect_tests(crate_filter: Option<&str>) -> BTreeMap<&'static str, Vec<TestDescriptor>> {
    TEST_FAILED_FLAG.store(false, Ordering::Relaxed);

    let tests = get_tests();
    if tests.is_empty() {
        return BTreeMap::new();
    }

    let crate_filters = normalize_crate_filter(crate_filter);

    let mut grouped: BTreeMap<&'static str, Vec<TestDescriptor>> = BTreeMap::new();
    for test in tests {
        if crate_filters.is_empty()
            || crate_filters
                .iter()
                .any(|cn| module_matches_crate(test.module, cn))
        {
            grouped.entry(test.module).or_default().push(test);
        }
    }

    grouped
}
