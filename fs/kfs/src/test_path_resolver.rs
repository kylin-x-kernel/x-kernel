//! Unit tests for PathResolver.

#![cfg(unittest)]

use unittest::def_test;

use crate::PathResolver;

#[def_test]
fn test_path_resolver_max_symlinks_config() {
    // Test custom max symlinks configuration
    let _resolver = PathResolver::with_max_symlinks(10);
    // Just verify it can be created

    let _resolver = PathResolver::with_max_symlinks(100);

    // Test default
    let _resolver = PathResolver::new();
}

#[def_test]
fn test_path_resolver_clone() {
    // Test that PathResolver can be cloned
    let resolver1 = PathResolver::with_max_symlinks(20);
    let _resolver2 = resolver1.clone();
}
