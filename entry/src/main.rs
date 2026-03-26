// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

#![no_std]
#![no_main]
#![doc = include_str!("../../README.md")]

#[macro_use]
extern crate klogger;

extern crate alloc;
extern crate kruntime;

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
mod entry;

#[cfg(not(feature = "unittest"))]
pub const CMDLINE: &[&str] = &["/bin/sh", "-c", include_str!("init.sh")];

#[cfg(not(feature = "unittest"))]
#[unsafe(no_mangle)]
fn main() {
    use alloc::{borrow::ToOwned, vec::Vec};

    use kfs::FS_CONTEXT;
    kserveices::init();

    let args = CMDLINE
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let envs = [];

    let exit_code = entry::run_initproc(&args, &envs);
    info!("Init process exited with code: {exit_code:?}");

    let cx = FS_CONTEXT.lock();
    cx.root_dir()
        .unmount_all()
        .expect("Failed to unmount all filesystems");
    cx.root_dir()
        .filesystem()
        .flush()
        .expect("Failed to flush rootfs");
}

#[cfg(feature = "unittest")]
#[unsafe(no_mangle)]
fn main() {
    kserveices::init();
    kserveices::register_unittest_runtime();

    {
        let cx = kfs::FS_CONTEXT.lock();
        let root = cx.root_dir().clone();
        let fs_ops = kfs::FsOperations::new(root);

        warn!("Cleaning up stale coverage data if exists...");
        if let Err(err) = fs_ops.remove_file("/.llvm-cov/default.profraw") {
            if err.canonicalize() != fs_ng_vfs::VfsError::NotFound {
                warn!("Failed to remove stale coverage data: {:?}", err);
            }
        }
    }

    use alloc::{sync::Arc, vec::Vec};
    use core::sync::atomic::{AtomicBool, Ordering};

    use ktask::spawn;

    let finished = Arc::new(AtomicBool::new(false));
    let finished_clone = finished.clone();

    spawn(move || {
        let crate_filter = unittest_crate_filter();
        if let Some(crate_filter) = crate_filter {
            warn!("Running unit tests with crate filter: {}", crate_filter);
        }
        let test_passed = unittest::test_run_ok_filtered(crate_filter);

        if test_passed {
            warn!("=== UNITTEST_STATUS: ALL_TESTS_PASSED ===");
        } else {
            warn!("=== UNITTEST_STATUS: TESTS_FAILED ===");
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
    if let Err(e) = unsafe { minicov::capture_coverage(&mut cov) } {
        error!("capture_coverage failed: {:?}", e);
    } else if !cov.is_empty() {
        let cx = kfs::FS_CONTEXT.lock();
        let root = cx.root_dir().clone();
        let fs_ops = kfs::FsOperations::new(root);

        let _ = fs_ops.create_dir("/.llvm-cov", fs_ng_vfs::NodePermission::default());
        if let Err(e) = fs_ops.write("/.llvm-cov/default.profraw", &cov) {
            error!("Failed to write coverage data: {:?}", e);
        } else {
            info!(
                "Coverage data successfully written! Size: {} bytes",
                cov.len()
            );
        }

        if let Err(e) = cx.root_dir().filesystem().flush() {
            error!("Failed to flush filesystem: {:?}", e);
        }
    } else {
        info!("No coverage data to write.");
    }

    info!("Unit tests completed, shutting down...");
    khal::power::shutdown();
}

#[cfg(feature = "aarch64_crosvm_virt")]
extern crate aarch64_crosvm_virt;
#[cfg(feature = "aarch64_qemu_virt")]
extern crate aarch64_qemu_virt;
#[cfg(feature = "loongarch64_qemu_virt")]
extern crate loongarch64_qemu_virt;
#[cfg(feature = "riscv64_qemu_virt")]
extern crate riscv64_qemu_virt;
#[cfg(feature = "x86_64_qemu_virt")]
extern crate x86_64_qemu_virt;
#[cfg(feature = "x86_csv")]
extern crate x86_csv;
