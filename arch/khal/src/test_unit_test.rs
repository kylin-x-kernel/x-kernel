use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

#[cfg(feature = "paging")]
use crate::paging::tests_paging::TEST_PAGING;
#[cfg(feature = "tls")]
use crate::tls::tests_tls::TEST_TLS;
use crate::{
    dtb::tests_dtb::TEST_DTB, irq::tests_irq::TEST_IRQ, mem::tests_mem::TEST_MEM,
    percpu::tests_percpu::TEST_PERCPU, time::tests_time::TEST_TIME,
};

/// Run khal unit tests.
pub fn khal_unit_test() {
    warn!("********************************");
    warn!("Starting khal unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, TEST_DTB);
    run_tests!(runner, TEST_IRQ);
    run_tests!(runner, TEST_MEM);
    run_tests!(runner, TEST_PERCPU);
    run_tests!(runner, TEST_TIME);

    #[cfg(feature = "tls")]
    run_tests!(runner, TEST_TLS);

    #[cfg(feature = "paging")]
    run_tests!(runner, TEST_PAGING);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
    warn!("********************************");
}
