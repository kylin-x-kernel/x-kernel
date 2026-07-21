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

mod runtime;
#[cfg(feature = "unittest")]
mod unittest_simple;

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

#[cfg(not(feature = "unittest"))]
pub const CMDLINE: &[&str] = &["/bin/sh", "-c", include_str!("init.sh")];

fn print_boot_info() {
    const fn configured_log_level() -> &'static str {
        if kbuild_config::LOG_LEVEL_ERROR {
            "error"
        } else if kbuild_config::LOG_LEVEL_WARN {
            "warn"
        } else if kbuild_config::LOG_LEVEL_INFO {
            "info"
        } else if kbuild_config::LOG_LEVEL_DEBUG {
            "debug"
        } else if kbuild_config::LOG_LEVEL_TRACE {
            "trace"
        } else {
            "off"
        }
    }

    kprintln!(
        indoc::indoc! {"
            arch = {}
            platform = {}
            target = {}
            build_mode = {}
            build_machine = {}
            build_time = {}
            log_level = {}
            backtrace = {}
            smp = {}
            virt = {}
        "},
        kbuild_config::ARCH,
        kbuild_config::PLATFORM,
        option_env!("K_TARGET").unwrap_or(""),
        option_env!("K_MODE").unwrap_or(""),
        option_env!("KBUILD_BUILD_MACHINE").unwrap_or("unknown"),
        option_env!("KBUILD_BUILD_TIME").unwrap_or("unknown"),
        configured_log_level(),
        backtrace::is_enabled(),
        kcpu_id_map::nr_cpus(),
        if kbuild_config::KFEAT_VMM {
            "on"
        } else {
            "off"
        },
    );
}

#[cfg(not(feature = "unittest"))]
#[unsafe(no_mangle)]
fn main() {
    use alloc::{borrow::ToOwned, vec::Vec};

    print_boot_info();

    runtime::init_runtime();

    if kbuild_config::KFEAT_VMM && !kvmm::selftest::vmm_selftest_smp() {
        error!("VMM selftest failed");
    }

    if kbuild_config::KFEAT_VMM && !kvmm::selftest::vmm_selftest_guest_mem() {
        error!("VMM guest-memory selftest failed");
    }

    if kbuild_config::KFEAT_VMM && !kvmm::selftest::vmm_selftest_multi_vm() {
        error!("VMM multi-VM selftest failed");
    }

    let args = CMDLINE
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let envs = [];

    let exit_code = posix_process::run_init_process(&args, &envs, ksyscall::dispatch_irq_syscall);
    info!("Init process exited with code: {exit_code:?}");

    let namespace = kvfs::MntNamespace::initial().expect("mount namespace must be initialized");
    kvfs::sync_filesystems().expect("Failed to flush mounted filesystems");
    let root = namespace.visible_root_path();
    namespace
        .detach_tree(&root)
        .expect("Failed to unmount all filesystems");
    root.sync_filesystem().expect("Failed to flush rootfs");

    info!("Init process finished, powering off...");
    khal::power::shutdown();
}

#[cfg(feature = "unittest")]
#[unsafe(no_mangle)]
fn main() {
    use alloc::{sync::Arc, vec::Vec};
    use core::sync::atomic::{AtomicBool, Ordering};

    use ktask::spawn;

    print_boot_info();

    runtime::init_runtime();
    runtime::register_unittest_runtime();

    {
        let fs_struct = fs_context::init_fs().lock().clone_for_process();
        let cred = kcred::initial_cred();

        warn!("Cleaning up stale coverage data if exists...");
        let remove_result = kvfs::Filename::new("/.llvm-cov/default.profraw")
            .lookup_at(
                fs_struct.root(),
                fs_struct.pwd(),
                kvfs::LookupIntent::Open,
                kvfs::LookupFlags::no_follow(),
                &cred,
            )
            .and_then(|file| {
                let name = file.name();
                file.parent()
                    .ok_or(kvfs::VfsError::IsADirectory)?
                    .unlink(&name, &cred)
            });
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
            unittest::print_unittest_message(format_args!(
                "Running unit tests with crate filter: {}",
                crate_filter
            ));
        }

        let grouped = unittest::collect_tests(crate_filter);

        if grouped.is_empty() {
            unittest::print_unittest_message(format_args!("No tests found!"));
            unittest::print_unittest_status(false);
            finished_clone.store(true, Ordering::Release);
            return;
        }

        let total_tests: usize = grouped.values().map(|v| v.len()).sum();
        unittest::print_unittest_message(format_args!("================================"));
        unittest::print_unittest_message(format_args!(
            "Starting unit tests [unittest] (parallel)..."
        ));
        unittest::print_unittest_message(format_args!(
            "  {} module(s), {} test(s)",
            grouped.len(),
            total_tests
        ));
        unittest::print_unittest_message(format_args!("================================"));

        let passed = Arc::new(AtomicUsize::new(0));
        let failed = Arc::new(AtomicUsize::new(0));
        let ignored = Arc::new(AtomicUsize::new(0));

        // Flatten to a Vec of &'static TestDescriptor to satisfy 'static bound.
        let flat: Vec<&'static unittest::TestDescriptor> = grouped
            .iter()
            .flat_map(|(_, tests)| tests.iter().copied())
            .collect();

        for (module, tests) in &grouped {
            unittest::print_unittest_message(format_args!(
                "  [{}] ({} tests)",
                module,
                tests.len()
            ));
        }

        // Split into serial and parallel tests.
        // Serial: explicitly marked serial OR user execution mode.
        // Only Standard-mode tests without serial flag run in parallel.
        let (serial_tests, parallel_tests): (
            Vec<&'static unittest::TestDescriptor>,
            Vec<&'static unittest::TestDescriptor>,
        ) = flat
            .into_iter()
            .partition(|t| t.serial || t.execution_mode != unittest::TestExecutionMode::Standard);

        // Run serial tests sequentially first
        if !serial_tests.is_empty() {
            unittest::print_unittest_message(format_args!(
                "  Running {} serial test(s)...",
                serial_tests.len()
            ));
            for test in &serial_tests {
                let module_name = test.module;
                let test_name = test.name();
                let result = test.run();
                match result {
                    TestResult::Ok => {
                        passed.fetch_add(1, Ordering::Relaxed);
                        unittest::print_unittest_message(format_args!(
                            "      {}::{} ... ok",
                            module_name, test_name
                        ));
                    }
                    TestResult::Failed => {
                        failed.fetch_add(1, Ordering::Relaxed);
                        unittest::print_unittest_error(format_args!(
                            "      {}::{} ... FAILED",
                            module_name, test_name
                        ));
                    }
                    TestResult::Ignored => {
                        ignored.fetch_add(1, Ordering::Relaxed);
                        unittest::print_unittest_message(format_args!(
                            "      {}::{} ... ignored",
                            module_name, test_name
                        ));
                    }
                }
            }
        }

        // Run parallel tests concurrently
        let mut tasks: Vec<ktask::KtaskRef> = Vec::with_capacity(parallel_tests.len());
        for test in parallel_tests {
            let p = passed.clone();
            let f = failed.clone();
            let ig = ignored.clone();
            let module_name = test.module;
            let test_name = test.name();

            tasks.push(task_spawn(move || {
                let result = test.run();
                match result {
                    TestResult::Ok => {
                        p.fetch_add(1, Ordering::Relaxed);
                        unittest::print_unittest_message(format_args!(
                            "      {}::{} ... ok",
                            module_name, test_name
                        ));
                    }
                    TestResult::Failed => {
                        f.fetch_add(1, Ordering::Relaxed);
                        unittest::print_unittest_error(format_args!(
                            "      {}::{} ... FAILED",
                            module_name, test_name
                        ));
                    }
                    TestResult::Ignored => {
                        ig.fetch_add(1, Ordering::Relaxed);
                        unittest::print_unittest_message(format_args!(
                            "      {}::{} ... ignored",
                            module_name, test_name
                        ));
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

        unittest::print_unittest_message(format_args!(""));
        unittest::print_unittest_message(format_args!(
            "  >>> Test results: {} passed, {} failed, {} ignored, {} total",
            p, f, ig, total
        ));

        let test_passed = f == 0;
        unittest::print_unittest_status(test_passed);

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
            let _ = kvfs::Filename::new("/.llvm-cov")
                .create_at(
                    fs_struct.root(),
                    fs_struct.pwd(),
                    kvfs::LookupIntent::Open,
                    kvfs::LookupFlags::DIRECTORY,
                    &cred,
                )
                .and_then(|(dir, name)| dir.mkdir(&name, kvfs::NodePermission::default(), &cred));
            let file = kvfs::Filename::new("/.llvm-cov/default.profraw").open_with_flags_at(
                fs_struct.root(),
                fs_struct.pwd(),
                linux_raw_sys::general::O_WRONLY
                    | linux_raw_sys::general::O_CREAT
                    | linux_raw_sys::general::O_TRUNC,
                kvfs::NodePermission::from_bits_truncate(0o644),
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
    khal::power::shutdown();
}
