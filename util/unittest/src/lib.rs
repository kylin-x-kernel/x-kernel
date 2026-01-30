//! Minimal unit-test framework for kernel components.
#![no_std]

#[macro_use]
extern crate log;
extern crate alloc;

pub mod runner;
pub mod test_examples;
pub mod test_framework;
pub mod test_framework_basic;

// Re-export the def_test macro from unittest-macros crate
pub use macros::def_test;
// Re-export the test runner function
pub use runner::{test_run, test_run_ok};
// Re-export commonly used types
pub use test_framework::{TestDescriptor, TestRunner, TestStats, Testable};
pub use test_framework_basic::TestResult;
