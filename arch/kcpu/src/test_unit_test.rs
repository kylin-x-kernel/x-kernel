use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

#[cfg(all(feature = "kcpu_test", target_arch = "aarch64"))]
use crate::aarch64::tests_arch::TEST_ARCH_AARCH64;
#[cfg(feature = "uspace")]
use crate::userspace_common::tests_userspace_common::TEST_USERSPACE_COMMON;
#[cfg(all(feature = "kcpu_test", target_arch = "x86_64"))]
use crate::x86_64::tests_arch::TEST_ARCH_X86_64;
use crate::{
    active_exception_context::tests_active_exception_context::TEST_ACTIVE_EXCEPTION_CONTEXT,
    excp::tests_excp::TEST_EXCP,
};

/// Run kcpu unit tests.
pub fn kcpu_unit_test() {
    warn!("********************************");
    warn!("Starting kcpu unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, TEST_ACTIVE_EXCEPTION_CONTEXT);
    run_tests!(runner, TEST_EXCP);

    #[cfg(feature = "uspace")]
    run_tests!(runner, TEST_USERSPACE_COMMON);

    #[cfg(target_arch = "aarch64")]
    run_tests!(runner, TEST_ARCH_AARCH64);

    #[cfg(target_arch = "x86_64")]
    run_tests!(runner, TEST_ARCH_X86_64);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
    warn!("********************************");
}
