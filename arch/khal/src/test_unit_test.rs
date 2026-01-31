use unittest::test_run;

/// Run khal unit tests.
pub fn khal_unit_test() {
    warn!("********************************");
    warn!("Starting khal unit tests...");

    let stats = test_run();
    if stats.failed > 0 {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
    warn!("********************************");
}
