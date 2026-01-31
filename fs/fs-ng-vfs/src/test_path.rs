//! Unit tests for Path and PathBuf.

#![cfg(unittest)]

extern crate alloc;
use alloc::vec::Vec;

use unittest::{assert_eq, def_test};

use crate::path::Path;

#[def_test]
fn test_path_normalization_complex() {
    // Test complex path normalization with multiple .. and .
    let path = Path::new("/foo/bar/../baz/./qux/../file.txt");
    let normalized = path.normalize().unwrap();
    assert_eq!(normalized.as_str(), "/foo/baz/file.txt");

    // Test edge case: too many parent references
    let path = Path::new("/../../../test");
    assert!(path.normalize().is_none());

    // Test with only dots
    let path = Path::new("././././test");
    let normalized = path.normalize().unwrap();
    assert_eq!(normalized.as_str(), "/test");
}

#[def_test]
fn test_path_components_bidirectional() {
    // Test forward and backward iteration produce same components
    let test_paths = [
        "/foo/bar/baz",
        "../relative/path",
        "./current/dir",
        "foo/bar/../baz",
        "/",
        ".",
        "..",
    ];

    for path_str in test_paths {
        let path = Path::new(path_str);
        let forward: Vec<_> = path.components().collect();
        let mut backward: Vec<_> = path.components().rev().collect();
        backward.reverse();
        assert_eq!(forward, backward, "Failed for path: {path_str}");
    }
}
