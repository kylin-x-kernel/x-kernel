//! Unit tests for WorkingContext.

#![cfg(unittest)]

use unittest::{TestResult, assert, def_test};

use crate::WorkingContext;

/// Helper function to create a test filesystem with ramdisk
#[cfg(feature = "fat")]
fn create_test_fs() -> fs_ng_vfs::Filesystem {
    extern crate alloc;
    use alloc::boxed::Box;

    use block::ramdisk::RamDisk;
    use kdriver::prelude::*;

    // Create a 2MB ramdisk
    let ramdisk = RamDisk::new(2 * 1024 * 1024);
    let dev = Box::new(ramdisk);
    let block_dev = BlockDevice::new(dev);

    // Create FAT filesystem on the ramdisk
    crate::fs::fat::FatFilesystem::new(block_dev)
}

#[cfg(feature = "fat")]
#[def_test]
fn test_working_context_new() -> TestResult {
    // Create a test filesystem
    let fs = create_test_fs();
    let mp = fs_ng_vfs::Mountpoint::new_root(&fs);
    let root_loc = mp.root_location();

    // Create a new working context
    let ctx = WorkingContext::new(root_loc.clone());

    // Verify root and cwd point to the same location initially
    assert!(ctx.root().entry().ptr_eq(root_loc.entry()));
    assert!(ctx.cwd().entry().ptr_eq(root_loc.entry()));

    TestResult::Ok
}

#[cfg(feature = "fat")]
#[def_test]
fn test_working_context_clone() -> TestResult {
    // Create a test filesystem
    let fs = create_test_fs();
    let mp = fs_ng_vfs::Mountpoint::new_root(&fs);
    let root_loc = mp.root_location();

    // Create a working context and clone it
    let ctx1 = WorkingContext::new(root_loc);
    let ctx2 = ctx1.clone();

    // Verify both contexts point to the same root
    assert!(ctx1.root().entry().ptr_eq(ctx2.root().entry()));
    assert!(ctx1.cwd().entry().ptr_eq(ctx2.cwd().entry()));

    TestResult::Ok
}

#[cfg(feature = "fat")]
#[def_test]
fn test_working_context_chdir() -> TestResult {
    use fs_ng_vfs::OpenOptions;

    // Create a test filesystem
    let fs = create_test_fs();
    let mp = fs_ng_vfs::Mountpoint::new_root(&fs);
    let root_loc = mp.root_location();

    // Create a subdirectory
    let subdir = root_loc
        .create("testdir", OpenOptions::dir().create(true))
        .expect("Failed to create directory");

    // Create working context and change directory
    let mut ctx = WorkingContext::new(root_loc.clone());
    ctx.chdir(subdir.clone()).expect("chdir failed");

    // Verify cwd changed but root stayed the same
    assert!(ctx.root().entry().ptr_eq(root_loc.entry()));
    assert!(ctx.cwd().entry().ptr_eq(subdir.entry()));

    TestResult::Ok
}

#[cfg(feature = "fat")]
#[def_test]
fn test_working_context_with_cwd() -> TestResult {
    use fs_ng_vfs::OpenOptions;

    // Create a test filesystem
    let fs = create_test_fs();
    let mp = fs_ng_vfs::Mountpoint::new_root(&fs);
    let root_loc = mp.root_location();

    // Create a subdirectory
    let subdir = root_loc
        .create("testdir2", OpenOptions::dir().create(true))
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

#[cfg(not(feature = "fat"))]
#[def_test]
fn test_working_context_basic() -> TestResult {
    // When FAT feature is not enabled, just verify basic type properties
    fn assert_clone<T: Clone>() {}
    fn assert_debug<T: core::fmt::Debug>() {}

    assert_clone::<WorkingContext>();
    assert_debug::<WorkingContext>();

    TestResult::Ok
}
