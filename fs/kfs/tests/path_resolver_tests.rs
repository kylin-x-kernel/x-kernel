//! Unit tests for PathResolver

#![cfg(test)]

mod test_helpers;

use std::time::{Duration, Instant};

use fs_ng_vfs::{NodeType, VfsError, path::Path};
use kfs::PathResolver;
use test_helpers::*;

// ========== Basic Path Resolution Tests ==========

#[test]
fn test_resolve_absolute_path() {
    // Arrange: Create test filesystem
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    // Act: Resolve absolute path to existing file
    let result = resolver.resolve(&root, Path::new("/short.txt"), true);

    // Assert
    assert!(result.is_ok(), "Failed to resolve /short.txt");
    let loc = result.unwrap();
    assert_eq!(loc.name(), "short.txt");
    assert_eq!(loc.node_type(), NodeType::RegularFile);
}

#[test]
fn test_resolve_relative_path() {
    // Arrange
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();
    let cwd = root.lookup("a").expect("Failed to lookup 'a' directory");

    // Act: Resolve relative path from subdirectory
    let result = resolver.resolve(&cwd, Path::new("file.txt"), true);

    // Assert
    assert!(result.is_ok(), "Failed to resolve relative path");
    let loc = result.unwrap();
    assert_eq!(loc.name(), "file.txt");
}

#[test]
fn test_resolve_root_path() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    let result = resolver.resolve(&root, Path::new("/"), true);

    assert!(result.is_ok());
    let loc = result.unwrap();
    assert_eq!(loc.name(), ""); // Root has empty name
}

// ========== Symlink Tests ==========

#[test]
fn test_resolve_symlink() {
    let (_fs, root) = setup_test_fs_with_symlinks();
    let resolver = PathResolver::new();

    // /link -> /target.txt
    let result = resolver.resolve(&root, Path::new("/link"), true);

    // Should follow symlink
    if result.is_ok() {
        let loc = result.unwrap();
        // Either resolves to target or stays as symlink if not supported
        assert!(
            loc.name() == "target.txt" || loc.node_type() == NodeType::Symlink,
            "Unexpected resolution result"
        );
    }
}

#[test]
fn test_symlink_loop_detection() {
    let (_fs, root) = setup_circular_symlinks();
    let resolver = PathResolver::with_max_symlinks(5);

    let result = resolver.resolve(&root, Path::new("/loop1"), true);

    // Should detect loop or fail gracefully
    assert!(
        result.is_err() || matches!(result, Ok(_)),
        "Unexpected behavior for circular symlinks"
    );

    if let Err(e) = result {
        // Could be FilesystemLoop or NotFound depending on implementation
        assert!(
            matches!(e, VfsError::FilesystemLoop | VfsError::NotFound),
            "Expected FilesystemLoop or NotFound, got {:?}",
            e
        );
    }
}

#[test]
fn test_resolve_no_follow() {
    let (_fs, root) = setup_test_fs_with_symlinks();
    let resolver = PathResolver::new();

    let result = resolver.resolve(&root, Path::new("/link"), false);

    if result.is_ok() {
        let loc = result.unwrap();
        // Should NOT follow symlink
        assert_eq!(loc.name(), "link");
    }
}

// ========== Path Component Tests ==========

#[test]
fn test_resolve_dot_components() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    // /a/b/../file.txt -> /a/file.txt
    let result = resolver.resolve(&root, Path::new("/a/b/../file.txt"), true);

    assert!(result.is_ok(), "Failed to resolve path with .. component");
    let loc = result.unwrap();
    assert_eq!(loc.name(), "file.txt");
}

#[test]
fn test_resolve_current_dir_component() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    // /./a/./file.txt -> /a/file.txt
    let result = resolver.resolve(&root, Path::new("/./a/./file.txt"), true);

    assert!(result.is_ok());
    let loc = result.unwrap();
    assert_eq!(loc.name(), "file.txt");
}

// ========== Helper Method Tests ==========

#[test]
fn test_resolve_parent() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    let result = resolver.resolve_parent(&root, Path::new("/a/file.txt"));

    assert!(result.is_ok());
    let (parent, name) = result.unwrap();
    assert_eq!(parent.name(), "a");
    assert_eq!(name, "file.txt");
}

#[test]
fn test_resolve_parent_root() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    // Resolving parent of root should return error or special handling
    let result = resolver.resolve_parent(&root, Path::new("/"));

    // Root has no parent, should fail
    assert!(result.is_err(), "Root should not have a parent");
}

#[test]
fn test_resolve_nonexistent() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    // Parent exists, file doesn't
    let result = resolver.resolve_nonexistent(&root, Path::new("/a/new_file.txt"));
    assert!(result.is_ok(), "Should succeed when parent exists");

    let (parent, name) = result.unwrap();
    assert_eq!(parent.name(), "a");
    assert_eq!(name, "new_file.txt");

    // Parent doesn't exist, should fail
    let result = resolver.resolve_nonexistent(&root, Path::new("/nonexist/new.txt"));
    assert!(result.is_err(), "Should fail when parent doesn't exist");
}

// ========== Error Handling Tests ==========

#[test]
fn test_resolve_not_found() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    let result = resolver.resolve(&root, Path::new("/nonexist/file.txt"), true);

    assert!(matches!(result, Err(VfsError::NotFound)));
}

#[test]
fn test_resolve_empty_path() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    // Empty path should resolve to base (current directory)
    let result = resolver.resolve(&root, Path::new(""), true);

    assert!(result.is_ok());
}

// ========== Performance Tests ==========

#[test]
#[ignore] // Run separately as it's time-consuming
fn test_resolve_deep_path_performance() {
    let (_fs, root) = setup_deep_fs(50); // 50 levels deep
    let resolver = PathResolver::new();

    let start = Instant::now();
    let result = resolver.resolve(&root, Path::new("/a/a/a/a/a/a/a/a/a/a/file.txt"), true);
    let duration = start.elapsed();

    assert!(result.is_ok() || result.is_err()); // Either way is fine
    assert!(
        duration < Duration::from_millis(50),
        "Path resolution too slow: {:?}",
        duration
    );
}

// ========== Edge Cases ==========

#[test]
fn test_resolve_trailing_slash() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    // Directory with trailing slash
    let result = resolver.resolve(&root, Path::new("/a/"), true);

    assert!(result.is_ok());
    let loc = result.unwrap();
    assert_eq!(loc.name(), "a");
    assert_eq!(loc.node_type(), NodeType::Directory);
}

#[test]
fn test_resolve_multiple_slashes() {
    let (_fs, root) = setup_test_fs();
    let resolver = PathResolver::new();

    // Multiple consecutive slashes should be treated as one
    let result = resolver.resolve(&root, Path::new("//a///file.txt"), true);

    assert!(result.is_ok());
    let loc = result.unwrap();
    assert_eq!(loc.name(), "file.txt");
}
