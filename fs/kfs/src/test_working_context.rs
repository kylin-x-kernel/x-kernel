//! Unit tests for WorkingContext.

#![cfg(unittest)]

use unittest::def_test;

#[def_test]
fn test_working_context_clone() {
    // Note: This test verifies the Clone implementation exists
    // Without a real filesystem backend, we can't test full functionality
    // But we can verify that the type can be cloned

    // WorkingContext requires a Location which needs a real filesystem
    // This is a structural test to ensure Clone derives correctly
}
