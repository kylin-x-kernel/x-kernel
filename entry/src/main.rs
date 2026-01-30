//! SPDX-License-Identifier: Apache-2.0
//! Copyright (C) 2025-2026 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
//! See LICENSES for license details.

#![no_std]
#![no_main]
#![doc = include_str!("../../README.md")]

#[macro_use]
extern crate klogger;

extern crate alloc;
extern crate kruntime;

use alloc::{borrow::ToOwned, vec::Vec};

#[cfg(feature = "unittest")]
mod unittest_simple;

use kfs::FS_CONTEXT;

mod entry;

pub const CMDLINE: &[&str] = &["/bin/sh", "-c", include_str!("init.sh")];

#[unsafe(no_mangle)]
fn main() {
    kapi::init();

    let args = CMDLINE
        .iter()
        .copied()
        .map(str::to_owned)
        .collect::<Vec<_>>();
    let envs = [];

    #[cfg(feature = "unittest")]
    unittest::test_run();

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
