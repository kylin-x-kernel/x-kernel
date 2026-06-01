// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for WorkingContext.

#![cfg(unittest)]

use unittest::{TestResult, assert, def_test};

use crate::WorkingContext;

/// Helper function to create a test filesystem with ramdisk
fn create_test_fs() -> kvfs::Filesystem {
    extern crate alloc;
    use alloc::boxed::Box;

    use block::ramdisk::RamDisk;
    use kdriver::prelude::*;

    // Create a 2MB ramdisk
    let ramdisk = RamDisk::new(2 * 1024 * 1024);
    let _dev = Box::new(ramdisk);
    let block_dev = BlockDevice::new(1024);

    // Create FAT filesystem on the ramdisk
    crate::fs::fat::FatFilesystem::new(block_dev)
}

#[def_test]
fn test_working_context_new() -> TestResult {
    // Create a test filesystem
    let fs = create_test_fs();
    let mp = kvfs::Mountpoint::new_root(&fs);
    let root_loc = mp.root_location();

    // Create a new working context
    let ctx = WorkingContext::new(root_loc.clone());

    // Verify root and cwd point to the same location initially
    assert!(ctx.root().entry().ptr_eq(root_loc.entry()));
    assert!(ctx.cwd().entry().ptr_eq(root_loc.entry()));

    TestResult::Ok
}

#[def_test]
fn test_working_context_clone() -> TestResult {
    // Create a test filesystem
    let fs = create_test_fs();
    let mp = kvfs::Mountpoint::new_root(&fs);
    let root_loc = mp.root_location();

    // Create a working context and clone it
    let ctx1 = WorkingContext::new(root_loc);
    let ctx2 = ctx1.clone();

    // Verify both contexts point to the same root
    assert!(ctx1.root().entry().ptr_eq(ctx2.root().entry()));
    assert!(ctx1.cwd().entry().ptr_eq(ctx2.cwd().entry()));

    TestResult::Ok
}

#[def_test]
fn test_working_context_chdir() -> TestResult {
    use kvfs::{NodePermission, NodeType};

    // Create a test filesystem
    let fs = create_test_fs();
    let mp = kvfs::Mountpoint::new_root(&fs);
    let root_loc = mp.root_location();

    // Create a subdirectory
    let subdir = root_loc
        .create("testdir", NodeType::Directory, NodePermission::default())
        .expect("Failed to create directory");

    // Create working context and change directory
    let mut ctx = WorkingContext::new(root_loc.clone());
    ctx.chdir(subdir.clone()).expect("chdir failed");

    // Verify cwd changed but root stayed the same
    assert!(ctx.root().entry().ptr_eq(root_loc.entry()));
    assert!(ctx.cwd().entry().ptr_eq(subdir.entry()));

    TestResult::Ok
}

#[def_test]
fn test_working_context_with_cwd() -> TestResult {
    use kvfs::{NodePermission, NodeType};

    // Create a test filesystem
    let fs = create_test_fs();
    let mp = kvfs::Mountpoint::new_root(&fs);
    let root_loc = mp.root_location();

    // Create a subdirectory
    let subdir = root_loc
        .create("testdir2", NodeType::Directory, NodePermission::default())
        .expect("Failed to create directory");

    // Create working context and get a new context with different cwd
    let ctx1 = WorkingContext::new(root_loc.clone());
    let ctx2 = ctx1.with_cwd(subdir.clone()).expect("with_cwd failed");

    // Verify ctx1 is unchanged
    assert!(ctx1.cwd().entry().ptr_eq(root_loc.entry()));

    // Verify ctx2 has new cwd
    assert!(ctx2.cwd().entry().ptr_eq(subdir.entry()));
    assert!(ctx2.root().entry().ptr_eq(root_loc.entry()));

    TestResult::Ok
}
