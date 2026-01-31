use unittest::test_run;

/// Run kipi unit tests.
pub fn kipi_unit_test() {
    warn!("********************************");
    warn!("Starting kipi unit tests...");

    let stats = test_run();
    if stats.failed > 0 {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
    warn!("********************************");
}
