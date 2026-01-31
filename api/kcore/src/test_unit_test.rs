use unittest::test_run;

/// Runs kcore unit tests via the unittest runner.
pub fn kcore_unit_test() {
    warn!("********************************");
    warn!("Starting KCORE unit tests...");

    let stats = test_run();
    if stats.failed > 0 {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }

    warn!("********************************\n");
}
