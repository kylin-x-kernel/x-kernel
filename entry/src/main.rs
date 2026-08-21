// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![no_main]
#![doc = include_str!("../../README.md")]

#[macro_use]
extern crate klogger;

extern crate alloc;
extern crate kfeat;
extern crate kruntime;
extern crate kuaccess;

mod image_metadata;
mod runtime;
#[cfg(feature = "unittest")]
mod unittest_simple;

#[kiface::provide]
impl kruntime::SystemInitEntry {
    fn enter() {
        kernel_main()
    }
}

#[cfg(feature = "unittest")]
fn unittest_crate_filter() -> Option<&'static str> {
    match option_env!("UNITTEST_CRATE") {
        Some(raw) => {
            let trimmed = raw.trim();
            if trimmed.is_empty() {
                None
            } else {
                Some(trimmed)
            }
        }
        None => None,
    }
}

#[cfg(feature = "unittest")]
fn print_unittest(args: core::fmt::Arguments<'_>) {
    kprintln!("{}", args);
}

#[cfg(feature = "unittest")]
fn print_test_start(module_name: &str, test_name: &str) {
    unittest::ktest_println!("      START  {}::{}", module_name, test_name);
}

#[cfg(feature = "unittest")]
fn print_test_result(module_name: &str, test_name: &str, result: unittest::TestResult) {
    let status = match result {
        unittest::TestResult::Ok => "ok",
        unittest::TestResult::Failed => "FAILED",
        unittest::TestResult::Ignored => "ignored",
    };

    unittest::ktest_println!("      RESULT {}::{} ... {}", module_name, test_name, status);
}

#[cfg(not(feature = "unittest"))]
pub const CMDLINE: &[&str] = &["/bin/sh", "-c", include_str!("init.sh")];

fn print_boot_info() {
    image_metadata::print();
}

#[cfg(not(feature = "unittest"))]
fn kernel_main() {
    use alloc::{borrow::ToOwned, vec::Vec};

    print_boot_info();

    runtime::init_runtime();

    let args = CMDLINE
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let envs = [];

    // Spawn PID 1 as a fresh user task and return. This runs on the PID-less
    // late-init bootstrap thread, which is not transformed into init.
    posix_process::spawn_init_process(&args, &envs, ksyscall::dispatch_irq_syscall, || {
        if let Err(err) = kvfs::sync_filesystems() {
            warn!("sync filesystems after init exit failed: {err:?}");
        }
        if let Ok(namespace) = kvfs::MntNamespace::initial() {
            let root = namespace.visible_root_path();
            if let Err(err) = namespace.detach_tree(&root) {
                warn!("unmount all filesystems failed: {err:?}");
            }
            if let Err(err) = root.sync_filesystem() {
                warn!("flush rootfs failed: {err:?}");
            }
        }
        info!("Init process finished, powering off...");
        khal::power::power_off();
    });
}

#[cfg(feature = "unittest")]
fn kernel_main() {
    use alloc::{sync::Arc, vec::Vec};
    use core::sync::atomic::{AtomicBool, Ordering};

    use ktask::spawn;

    print_boot_info();

    runtime::init_runtime();
    runtime::register_unittest_runtime();
    unittest::set_printer(print_unittest);

    {
        let fs_struct = fs_context::init_fs().lock().clone_for_process();
        let cred = kcred::initial_cred();

        warn!("Cleaning up stale coverage data if exists...");
        let remove_result = kvfs::Filename::new("/.llvm-cov/default.profraw").unlink_at(
            fs_struct.root(),
            fs_struct.pwd(),
            &cred,
        );
        if let Err(err) = remove_result {
            if err.canonicalize() != kvfs::VfsError::NotFound {
                warn!("Failed to remove stale coverage data: {:?}", err);
            }
        }
    }

    let finished = Arc::new(AtomicBool::new(false));
    let finished_clone = finished.clone();

    spawn(move || {
        use core::sync::atomic::AtomicUsize;

        use ktask::spawn as task_spawn;
        use unittest::{TestResult, Testable};

        let crate_filter = unittest_crate_filter();
        if let Some(crate_filter) = crate_filter {
            unittest::ktest_println!("Running unit tests with crate filter: {}", crate_filter);
        }

        let grouped = unittest::collect_tests(crate_filter);

        if grouped.is_empty() {
            unittest::ktest_println!("No tests found!");
            unittest::ktest_println!("=== UNITTEST_STATUS: TESTS_FAILED ===");
            finished_clone.store(true, Ordering::Release);
            return;
        }

        let total_tests: usize = grouped.values().map(|v| v.len()).sum();
        unittest::ktest_println!("================================");
        unittest::ktest_println!("Starting unit tests [unittest] (parallel)...");
        unittest::ktest_println!("  {} module(s), {} test(s)", grouped.len(), total_tests);
        unittest::ktest_println!("================================");

        let passed = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let ignored = Arc::new(AtomicUsize::new(0));

        // Flatten to owned descriptors so test execution never reads the
        // writable linker registration section after discovery.
        let flat: Vec<unittest::TestDescriptor> = grouped
            .iter()
            .flat_map(|(_, tests)| tests.iter().copied())
            .collect();

        for (module, tests) in &grouped {
            unittest::ktest_println!("  [{}] ({} tests)", module, tests.len());
        }

        // Split into serial and parallel tests.
        // Serial: explicitly marked serial OR user execution mode.
        // Only Standard-mode tests without serial flag run in parallel.
        let (serial_tests, parallel_tests): (
            Vec<unittest::TestDescriptor>,
            Vec<unittest::TestDescriptor>,
        ) = flat
            .into_iter()
            .partition(|t| t.serial || t.execution_mode != unittest::TestExecutionMode::Standard);
        let mut failed_serial_tests = Vec::new();

        // Run serial tests sequentially first
        if !serial_tests.is_empty() {
            unittest::ktest_println!("  Running {} serial test(s)...", serial_tests.len());
            for test in &serial_tests {
                let module_name = test.module;
                let test_name = test.name();
                print_test_start(module_name, test_name);
                let result = test.run();
                print_test_result(module_name, test_name, result);
                match result {
                    TestResult::Ok => {
                        passed.fetch_add(1, Ordering::Relaxed);
                    }
                    TestResult::Failed => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        failed_serial_tests.push(*test);
                    }
                    TestResult::Ignored => {
                        ignored.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        }

        // Run parallel tests on a bounded worker set. Spawning every test at
        // once makes the runner sensitive to task-lifetime bugs and obscures
        // the failing descriptor when a worker faults before printing a result.
        let parallel_tests = Arc::new(parallel_tests);
        let parallel_test_failed = Arc::new(
            (0..parallel_tests.len())
                .map(|_| AtomicBool::new(false))
                .collect::<Vec<_>>(),
        );
        let next_test = Arc::new(AtomicUsize::new(0));
        let worker_count = core::cmp::min(
            parallel_tests.len(),
            core::cmp::max(1, kcpu_id_map::nr_cpus() * 4),
        );
        let mut tasks: Vec<ktask::KtaskRef> = Vec::with_capacity(worker_count);

        for _ in 0..worker_count {
            let tests = parallel_tests.clone();
            let next = next_test.clone();
            let p = passed.clone();
            let f = failed.clone();
            let ig = ignored.clone();
            let test_failed = parallel_test_failed.clone();

            tasks.push(task_spawn(move || {
                loop {
                    let test_idx = next.fetch_add(1, Ordering::Relaxed);
                    if test_idx >= tests.len() {
                        break;
                    }

                    let test = &tests[test_idx];
                    let module_name = test.module;
                    let test_name = test.name();
                    print_test_start(module_name, test_name);
                    let result = test.run();
                    print_test_result(module_name, test_name, result);
                    match result {
                        TestResult::Ok => {
                            p.fetch_add(1, Ordering::Relaxed);
                        }
                        TestResult::Failed => {
                            f.fetch_add(1, Ordering::Relaxed);
                            test_failed[test_idx].store(true, Ordering::Release);
                        }
                        TestResult::Ignored => {
                            ig.fetch_add(1, Ordering::Relaxed);
                        }
                    }
                }
            }));
        }

        for task in tasks {
            task.join();
        }

        let p = passed.load(Ordering::Relaxed);
        let f = failed.load(Ordering::Relaxed);
        let ig = ignored.load(Ordering::Relaxed);
        let total = p + f + ig;

        unittest::ktest_println!();
        unittest::ktest_println!(
            "  >>> Test results: {} passed, {} failed, {} ignored, {} total",
            p,
            f,
            ig,
            total
        );

        let test_passed = f == 0;
        if test_passed {
            unittest::ktest_println!("=== UNITTEST_STATUS: ALL_TESTS_PASSED ===");
        } else {
            unittest::ktest_println!("=== UNITTEST_STATUS: TESTS_FAILED ===");
            unittest::ktest_println!("=== FAILED_TESTS: {} ===", f);
            for test in &failed_serial_tests {
                unittest::ktest_println!("  {}::{}", test.module, test.name());
            }
            for (test_idx, test) in parallel_tests.iter().enumerate() {
                if parallel_test_failed[test_idx].load(Ordering::Acquire) {
                    unittest::ktest_println!("  {}::{}", test.module, test.name());
                }
            }
            unittest::ktest_println!("=== FAILED_TESTS_END ===");
        }

        finished_clone.store(true, Ordering::Release);
    });

    // Loop until tests are finished.
    // We use yield_now() to let the scheduler run the test task.
    while !finished.load(Ordering::Acquire) {
        ktask::yield_now();
    }

    info!("Writing LLVM coverage data to /.llvm-cov/default.profraw ...");
    let mut cov = Vec::new();
    if let Err(e) = xcover::write_profraw(&mut cov) {
        error!("write_profraw failed: {:?}", e);
    } else if !cov.is_empty() {
        let fs_struct = fs_context::init_fs().lock().clone_for_process();
        let cred = kcred::initial_cred();
        let write_result = (|| -> kvfs::VfsResult<()> {
            let _ = kvfs::Filename::new("/.llvm-cov").mkdir_at(
                fs_struct.root(),
                fs_struct.pwd(),
                kvfs::NodePermission::default(),
                kvfs::NodePermission::empty(),
                &cred,
            );
            let file = kvfs::Filename::new("/.llvm-cov/default.profraw").open_with_flags_at(
                fs_struct.root(),
                fs_struct.pwd(),
                linux_raw_sys::general::O_WRONLY
                    | linux_raw_sys::general::O_CREAT
                    | linux_raw_sys::general::O_TRUNC,
                kvfs::NodePermission::from_bits_truncate(0o644),
                kvfs::NodePermission::empty(),
                cred.clone(),
            )?;
            let mut pos = 0;
            file.write_from(cov.as_slice(), &mut pos)?;
            file.fsync(false)
        })();

        if let Err(e) = write_result {
            error!("Failed to write coverage data: {:?}", e);
        } else {
            info!(
                "Coverage data successfully written! Size: {} bytes",
                cov.len()
            );
        }

        if let Err(e) = kvfs::sync_filesystems() {
            error!("Failed to flush filesystem: {:?}", e);
        }
    } else {
        info!("No coverage data to write.");
    }

    info!("Unit tests completed, shutting down...");
    khal::power::power_off();
}
