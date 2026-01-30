use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

#[cfg(feature = "bitmap")]
use crate::bitmap::tests_bitmap::TEST_BITMAP;
#[cfg(feature = "buddy")]
use crate::buddy::tests_buddy::TEST_BUDDY;
#[cfg(feature = "slab")]
use crate::slab::tests_slab::TEST_SLAB;
#[cfg(feature = "tlsf")]
use crate::tlsf::tests_tlsf::TEST_TLSF;

/// Run alloc-engine unit tests.
pub fn alloc_engine_unit_test() {
    warn!("********************************");
    warn!("Starting alloc-engine unit tests...");

    let mut runner = TestRunner::new();

    #[cfg(feature = "bitmap")]
    run_tests!(runner, TEST_BITMAP);
    #[cfg(feature = "buddy")]
    run_tests!(runner, TEST_BUDDY);
    #[cfg(feature = "slab")]
    run_tests!(runner, TEST_SLAB);
    #[cfg(feature = "tlsf")]
    run_tests!(runner, TEST_TLSF);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
    warn!("********************************");
}
