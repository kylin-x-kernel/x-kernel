use unittest::test_run;

/// Run kcpu unit tests.
pub fn kcpu_unit_test() {
    warn!("********************************");
    warn!("Starting kcpu unit tests...");

    let stats = test_run();
    if stats.failed > 0 {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
    warn!("********************************");
}
