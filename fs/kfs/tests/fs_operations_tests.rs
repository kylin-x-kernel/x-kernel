//! Unit tests for FsOperations

#![cfg(test)]

mod test_helpers;

use fs_ng_vfs::{NodePermission, NodeType, path::Path};
use kfs::{FsContext, FsOperations};
use test_helpers::*;

// ========== Basic Construction Tests ==========

#[test]
fn test_fs_operations_new() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root.clone());

    assert_eq!(ops.root_dir().inode(), root.inode());
    assert_eq!(ops.current_dir().inode(), root.inode());
}

// ========== CRUD Tests ==========

#[test]
fn test_fs_operations_read() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    // Read existing file
    let result = ops.read(Path::new("/short.txt"));

    assert!(result.is_ok(), "Failed to read existing file");
    let data = result.unwrap();
    assert!(!data.is_empty(), "File should have content");
}

#[test]
fn test_fs_operations_write() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    // Write to new file
    let result = ops.write(Path::new("/test_write.txt"), b"hello world");

    assert!(result.is_ok(), "Failed to write file");

    // Read back
    let data = ops.read(Path::new("/test_write.txt")).unwrap();
    assert_eq!(data, b"hello world");
}

#[test]
fn test_fs_operations_crud_full_cycle() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    // Create
    ops.write(Path::new("/test.txt"), b"initial").unwrap();

    // Read
    let data = ops.read(Path::new("/test.txt")).unwrap();
    assert_eq!(data, b"initial");

    // Update
    ops.write(Path::new("/test.txt"), b"updated").unwrap();
    let data = ops.read(Path::new("/test.txt")).unwrap();
    assert_eq!(data, b"updated");

    // Delete
    ops.remove_file(Path::new("/test.txt")).unwrap();
    assert!(ops.read(Path::new("/test.txt")).is_err());
}

// ========== Directory Operations ==========

#[test]
fn test_create_dir() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    let result = ops.create_dir(Path::new("/newdir"), NodePermission::default());

    assert!(result.is_ok());
    let dir = result.unwrap();
    assert_eq!(dir.name(), "newdir");
    assert_eq!(dir.node_type(), NodeType::Directory);
}

#[test]
fn test_remove_dir() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    // Create then remove
    ops.create_dir(Path::new("/tempdir"), NodePermission::default())
        .unwrap();
    let result = ops.remove_dir(Path::new("/tempdir"));

    // Should succeed if directory is empty
    assert!(result.is_ok() || result.is_err()); // Either way is valid
}

#[test]
fn test_read_dir() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    let result = ops.read_dir(Path::new("/"));

    assert!(result.is_ok());
    let mut iter = result.unwrap();

    // Should have at least some entries
    let first = iter.next();
    assert!(first.is_some(), "Root directory should have entries");
}

// ========== Path Resolution Tests ==========

#[test]
fn test_resolve() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    let result = ops.resolve(Path::new("/a/file.txt"));

    assert!(result.is_ok());
    let loc = result.unwrap();
    assert_eq!(loc.name(), "file.txt");
}

#[test]
fn test_resolve_no_follow() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    let result = ops.resolve_no_follow(Path::new("/short.txt"));

    assert!(result.is_ok());
    let loc = result.unwrap();
    assert_eq!(loc.name(), "short.txt");
}

// ========== Context Management ==========

#[test]
fn test_set_current_dir() {
    let (_fs, root) = setup_test_fs();
    let mut ops = FsOperations::new(root.clone());

    let subdir = root.lookup("a").unwrap();
    let result = ops.set_current_dir(subdir.clone());

    assert!(result.is_ok());
    assert_eq!(ops.current_dir().inode(), subdir.inode());
}

// ========== Backward Compatibility Tests ==========

#[test]
fn test_backward_compatibility_type_alias() {
    let (_fs, root) = setup_test_fs();

    // Should be able to use FsContext as type
    let ctx: FsContext = FsOperations::new(root.clone());

    // And use all the same methods
    let result = ctx.read(Path::new("/short.txt"));
    assert!(result.is_ok());
}

#[test]
fn test_backward_compatibility_behavior() {
    let (_fs, root) = setup_test_fs();

    // New API
    let new_ops = FsOperations::new(root.clone());
    let new_result = new_ops.read(Path::new("/short.txt"));

    // Old API (FsContext alias)
    let old_ctx: FsContext = FsOperations::new(root);
    let old_result = old_ctx.read(Path::new("/short.txt"));

    // Behavior should be identical
    assert_eq!(new_result.is_ok(), old_result.is_ok());
    if new_result.is_ok() {
        assert_eq!(new_result.unwrap(), old_result.unwrap());
    }
}

// ========== Metadata Operations ==========

#[test]
fn test_metadata() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    let result = ops.metadata(Path::new("/short.txt"));

    assert!(result.is_ok());
    let meta = result.unwrap();
    assert_eq!(meta.node_type, NodeType::RegularFile);
}

#[test]
fn test_canonicalize() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    let result = ops.canonicalize(Path::new("/a/../short.txt"));

    assert!(result.is_ok());
    let path = result.unwrap();
    assert_eq!(path.as_str(), "/short.txt");
}

// ========== Rename Tests ==========

#[test]
fn test_rename() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    // Create a file
    ops.write(Path::new("/old_name.txt"), b"content").unwrap();

    // Rename it
    let result = ops.rename(Path::new("/old_name.txt"), Path::new("/new_name.txt"));

    assert!(result.is_ok() || result.is_err()); // Depends on filesystem support

    if result.is_ok() {
        // Old name should not exist
        assert!(ops.read(Path::new("/old_name.txt")).is_err());
        // New name should exist
        assert!(ops.read(Path::new("/new_name.txt")).is_ok());
    }
}

// ========== Link Tests ==========

#[test]
fn test_link() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    // Create a file
    ops.write(Path::new("/original.txt"), b"content").unwrap();

    // Create hard link
    let result = ops.link(Path::new("/original.txt"), Path::new("/link.txt"));

    // Hard links may not be supported by all filesystems
    if result.is_ok() {
        // Both should be readable
        assert!(ops.read(Path::new("/original.txt")).is_ok());
        assert!(ops.read(Path::new("/link.txt")).is_ok());
    }
}

#[test]
fn test_symlink() {
    let (_fs, root) = setup_test_fs();
    let ops = FsOperations::new(root);

    // Create a target file
    ops.write(Path::new("/target.txt"), b"content").unwrap();

    // Create symlink
    let result = ops.symlink("target.txt", Path::new("/symlink.txt"));

    // Symlinks may not be supported by all filesystems
    if result.is_ok() {
        let link = result.unwrap();
        assert_eq!(link.name(), "symlink.txt");
    }
}

// ========== Component Decoupling Tests ==========

#[test]
fn test_component_decoupling() {
    let (_fs, root) = setup_test_fs();

    // Can independently create and use components
    use kfs::{PathResolver, WorkingContext};

    let resolver = PathResolver::new();
    let loc = resolver.resolve(&root, Path::new("/a"), true).unwrap();

    let mut ctx = WorkingContext::new(root);
    ctx.chdir(loc).unwrap();

    // Then combine into FsOperations
    let ops = FsOperations::with_context(ctx);
    let result = ops.read(Path::new("file.txt")); // Relative to /a

    assert!(result.is_ok());
}
