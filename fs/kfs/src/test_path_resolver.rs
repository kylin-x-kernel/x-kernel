//! Unit tests for PathResolver.

#![allow(missing_docs)]

use unittest::{
    test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
};

use crate::PathResolver;

test_fn! {
    using TestResult;

    fn test_path_resolver_max_symlinks_config() {
        // Test custom max symlinks configuration
        let _resolver = PathResolver::with_max_symlinks(10);
        // Just verify it can be created

        let _resolver = PathResolver::with_max_symlinks(100);

        // Test default
        let _resolver = PathResolver::new();
    }
}

test_fn! {
    using TestResult;

    fn test_path_resolver_clone() {
        // Test that PathResolver can be cloned
        let resolver1 = PathResolver::with_max_symlinks(20);
        let _resolver2 = resolver1.clone();
    }
}

tests_name!(TEST_PATH_RESOLVER;
    test_path_resolver_max_symlinks_config,
    test_path_resolver_clone,
);
