// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! Unit tests for WorkingContext

#![cfg(test)]

mod test_helpers;

use fs_ng_vfs::{NodeType, VfsError};
use kfs::WorkingContext;
use test_helpers::*;

// ========== Construction Tests ==========

#[test]
fn test_working_context_new() {
    let (_fs, root) = setup_test_fs();
    let ctx = WorkingContext::new(root.clone());

    // Both root and cwd should point to the same location initially
    assert_eq!(ctx.root().inode(), root.inode());
    assert_eq!(ctx.cwd().inode(), root.inode());
}

// ========== Chdir Tests ==========

#[test]
fn test_chdir_to_subdirectory() {
    let (_fs, root) = setup_test_fs();
    let mut ctx = WorkingContext::new(root.clone());
    let subdir = root.lookup("a").expect("Failed to lookup subdirectory 'a'");

    let result = ctx.chdir(subdir.clone());

    assert!(result.is_ok(), "chdir should succeed");
    assert_eq!(ctx.cwd().inode(), subdir.inode(), "cwd should change");
    assert_eq!(ctx.root().inode(), root.inode(), "root should not change");
}

#[test]
fn test_chdir_to_file_fails() {
    let (_fs, root) = setup_test_fs();
    let mut ctx = WorkingContext::new(root.clone());

    // Try to find a file
    let file = root.lookup("short.txt").expect("Failed to lookup file");
    assert_eq!(file.node_type(), NodeType::RegularFile);

    let result = ctx.chdir(file);

    assert!(
        matches!(result, Err(VfsError::NotADirectory)),
        "chdir to file should fail with NotADirectory"
    );
}

#[test]
fn test_chdir_to_root() {
    let (_fs, root) = setup_test_fs();
    let mut ctx = WorkingContext::new(root.clone());

    // Change to subdirectory first
    let subdir = root.lookup("a").unwrap();
    ctx.chdir(subdir).unwrap();

    // Then change back to root
    let result = ctx.chdir(root.clone());

    assert!(result.is_ok());
    assert_eq!(ctx.cwd().inode(), root.inode());
}

// ========== With CWD Tests (Immutable) ==========

#[test]
fn test_with_cwd_immutable() {
    let (_fs, root) = setup_test_fs();
    let ctx = WorkingContext::new(root.clone());
    let subdir = root.lookup("a").expect("Failed to lookup subdirectory");

    let new_ctx = ctx
        .with_cwd(subdir.clone())
        .expect("with_cwd should succeed");

    // Original context should be unchanged
    assert_eq!(
        ctx.cwd().inode(),
        root.inode(),
        "Original cwd should not change"
    );
    // New context should have different cwd
    assert_eq!(
        new_ctx.cwd().inode(),
        subdir.inode(),
        "New cwd should be updated"
    );
    // Both should have same root
    assert_eq!(
        ctx.root().inode(),
        new_ctx.root().inode(),
        "Root should be same"
    );
}

#[test]
fn test_with_cwd_to_file_fails() {
    let (_fs, root) = setup_test_fs();
    let ctx = WorkingContext::new(root.clone());
    let file = root.lookup("short.txt").unwrap();

    let result = ctx.with_cwd(file);

    assert!(matches!(result, Err(VfsError::NotADirectory)));
}

// ========== Clone Tests ==========

#[test]
fn test_working_context_clone() {
    let (_fs, root) = setup_test_fs();
    let ctx1 = WorkingContext::new(root.clone());
    let ctx2 = ctx1.clone();

    assert_eq!(ctx1.root().inode(), ctx2.root().inode());
    assert_eq!(ctx1.cwd().inode(), ctx2.cwd().inode());
}

#[test]
fn test_clone_independence() {
    let (_fs, root) = setup_test_fs();
    let mut ctx1 = WorkingContext::new(root.clone());
    let mut ctx2 = ctx1.clone();

    // Change ctx1's cwd
    let subdir = root.lookup("a").unwrap();
    ctx1.chdir(subdir).unwrap();

    // ctx2 should be unaffected
    assert_eq!(
        ctx2.cwd().inode(),
        root.inode(),
        "Cloned context should be independent"
    );
}

// ========== Debug Tests ==========

#[test]
fn test_debug_format() {
    let (_fs, root) = setup_test_fs();
    let ctx = WorkingContext::new(root.clone());

    // Should implement Debug
    let debug_str = format!("{:?}", ctx);
    assert!(!debug_str.is_empty(), "Debug format should produce output");
}

// ========== Edge Cases ==========

#[test]
fn test_multiple_chdir() {
    let (_fs, root) = setup_test_fs();
    let mut ctx = WorkingContext::new(root.clone());

    // Navigate: root -> a -> b
    let a = root.lookup("a").unwrap();
    ctx.chdir(a.clone()).unwrap();
    assert_eq!(ctx.cwd().name(), "a");

    let b = a.lookup("b").unwrap();
    ctx.chdir(b.clone()).unwrap();
    assert_eq!(ctx.cwd().name(), "b");

    // Root should still be unchanged
    assert_eq!(ctx.root().inode(), root.inode());
}
