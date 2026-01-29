//! Test helper functions for creating mock filesystems

#![allow(unused)]

use std::collections::HashSet;

use fs_ng_vfs::{
    DirEntry, Filesystem, Location, Mountpoint, NodePermission, NodeType, VfsResult, path::Path,
};

/// Creates a basic test filesystem
/// Note: These tests will initially fail until we implement the new components
pub fn setup_test_fs() -> (Filesystem, Location) {
    // For now, create a minimal mock that will fail
    // This will be replaced with proper implementation once PathResolver etc are ready
    panic!(
        "setup_test_fs not yet implemented - waiting for PathResolver/WorkingContext/FsOperations \
         implementation"
    );
}

/// Creates a test filesystem with symlinks:
/// /
/// ├── target.txt
/// ├── link -> target.txt (symlink)
/// └── link_to_dir -> a/ (symlink to directory)
pub fn setup_test_fs_with_symlinks() -> (Filesystem, Location) {
    let (fs, root) = setup_test_fs();

    // Create target file
    if root.lookup("target.txt").is_err() {
        root.create(
            "target.txt",
            NodeType::RegularFile,
            NodePermission::default(),
        )
        .expect("Failed to create target.txt");
    }

    // Create symlink (if filesystem supports it)
    if let Ok(link) = root.create("link", NodeType::Symlink, NodePermission::default()) {
        let _ = link
            .entry()
            .as_file()
            .and_then(|f| f.set_symlink("target.txt"));
    }

    (fs, root)
}

/// Creates a filesystem with circular symlinks for loop detection:
/// /
/// ├── loop1 -> loop2
/// └── loop2 -> loop1
pub fn setup_circular_symlinks() -> (Filesystem, Location) {
    let (fs, root) = setup_test_fs();

    // Create circular symlinks if supported
    if let Ok(loop1) = root.create("loop1", NodeType::Symlink, NodePermission::default()) {
        let _ = loop1.entry().as_file().and_then(|f| f.set_symlink("loop2"));
    }
    if let Ok(loop2) = root.create("loop2", NodeType::Symlink, NodePermission::default()) {
        let _ = loop2.entry().as_file().and_then(|f| f.set_symlink("loop1"));
    }

    (fs, root)
}

/// Creates a deep directory structure for performance testing
/// /a/a/a/.../a/file.txt (depth levels)
pub fn setup_deep_fs(depth: usize) -> (Filesystem, Location) {
    let (fs, root) = setup_test_fs();

    let mut current = root.clone();
    for _ in 0..depth {
        match current.create("a", NodeType::Directory, NodePermission::default()) {
            Ok(dir) => current = dir,
            Err(_) => {
                // Directory might already exist
                if let Ok(dir) = current.lookup("a") {
                    current = dir;
                } else {
                    break;
                }
            }
        }
    }

    // Create a file at the deepest level
    let _ = current.create("file.txt", NodeType::RegularFile, NodePermission::default());

    (fs, root)
}
