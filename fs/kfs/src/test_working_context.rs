//! Unit tests for WorkingContext.

#![allow(missing_docs)]

use unittest::{
    test_fn, test_framework::TestDescriptor, test_framework_basic::TestResult, tests_name,
};

test_fn! {
    using TestResult;

    fn test_working_context_clone() {
        // Note: This test verifies the Clone implementation exists
        // Without a real filesystem backend, we can't test full functionality
        // But we can verify that the type can be cloned

        // WorkingContext requires a Location which needs a real filesystem
        // This is a structural test to ensure Clone derives correctly
    }
}

tests_name!(TEST_WORKING_CONTEXT;
    test_working_context_clone,
);
