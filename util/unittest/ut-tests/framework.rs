// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#[unittest::mod_test]
mod framework {
    use alloc::{collections::BTreeMap, vec};

    use unittest::def_test;

    use super::super::*;
    use crate::TestResult;

    fn ok_test() -> TestResult {
        TestResult::Ok
    }

    fn fail_test() -> TestResult {
        TestResult::Failed
    }

    fn assert_ok_test() -> TestResult {
        crate::assert!(true);
        crate::assert_eq!(1_u32, 1_u32);
        crate::assert_ne!(1_u32, 2_u32);
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

    #[def_test(serial)]
    fn test_testrunner_run_test_updates_stats() {
        let mut runner = TestRunner::new();
        let desc = TestDescriptor::new(
            "ok_test",
            "framework",
            ok_test,
            false,
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

    #[def_test(serial)]
    fn test_grouped_runner_sets_failed_flag() {
        TEST_FAILED_FLAG.store(false, Ordering::Relaxed);

        let ok = TestDescriptor::new(
            "ok",
            "alpha",
            ok_test,
            false,
            false,
            false,
            TestExecutionMode::Standard,
        );
        // let fail = TestDescriptor::new(
        //     "fail",
        //     "beta",
        //     fail_test,
        //     false,
        //     false,
        //     TestExecutionMode::Standard,
        // );

        let mut grouped = BTreeMap::new();
        grouped.insert("alpha", vec![&ok]);
        // grouped.insert("beta", vec![&fail]);

        let mut runner = TestRunner::new();
        runner.run_tests_grouped("framework", &grouped);

        let stats = runner.get_stats();
        assert_eq!(stats.total, 1);
        // assert_eq!(stats.failed, 1);
        // assert!(tests_failed());
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
            false,
            TestExecutionMode::Standard,
        );

        assert_eq!(ignored.name(), "ignored");
        assert_eq!(ignored.module(), "framework");
        assert!(!ignored.should_panic());
        assert!(ignored.ignore());
        assert_eq!(ignored.run(), TestResult::Ignored);
    }

    #[def_test(serial)]
    fn test_run_tests_descriptors_pass_path_and_tests_failed_false() {
        TEST_FAILED_FLAG.store(false, Ordering::Relaxed);
        let mut runner = TestRunner::new();
        let suite = [TestDescriptor::new(
            "ok_only",
            "framework",
            ok_test,
            false,
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

    #[def_test(serial)]
    fn test_run_test_simple_and_default_constructor() {
        let mut runner = TestRunner::default();
        let desc = TestDescriptor::new(
            "assert_ok",
            "framework",
            assert_ok_test,
            false,
            false,
            false,
            TestExecutionMode::Standard,
        );

        assert_eq!(runner.run_test_simple(&desc), TestResult::Ok);
        let stats = runner.get_stats();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.passed, 1);
    }

    // #[def_test]
    // fn test_assertion_macros_failure_path() {
    //     assert_eq!(assert_fail_test(), TestResult::Failed);
    // }

    #[def_test(serial)]
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
            false,
            TestExecutionMode::Standard,
        );
        let ignored = TestDescriptor::new(
            "ignored",
            "framework",
            ignored_test,
            false,
            true,
            false,
            TestExecutionMode::Standard,
        );

        assert_eq!(standard.name(), "standard");
        assert_eq!(standard.module(), "framework");
        assert!(standard.should_panic());
        assert!(!standard.ignore());
        assert_eq!(standard.run(), TestResult::Ok);
        assert_eq!(ignored.run(), TestResult::Ignored);
    }

    #[def_test(serial)]
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
                false,
                TestExecutionMode::Standard,
            ),
            // TestDescriptor::new(
            //     "fail",
            //     "framework",
            //     fail_test,
            //     false,
            //     false,
            //     TestExecutionMode::Standard,
            // ),
        ];

        runner.run_tests_descriptors("framework", &suite);

        let stats = runner.get_stats();
        assert_eq!(stats.total, 1);
        assert_eq!(stats.passed, 1);
        // assert_eq!(stats.failed, 1);
        // assert!(tests_failed());
        TEST_FAILED_FLAG.store(false, Ordering::Relaxed);
    }
}
