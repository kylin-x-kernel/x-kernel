use unittest::{
    run_tests,
    test_framework::{tests_failed, TestRunner},
};

use crate::{
    config::tests_config::TEST_CONFIG,
    futex::tests_futex::TEST_FUTEX,
    lrucache::tests_lrucache::TEST_LRUCACHE,
    mm::tests_mm::TEST_MM,
    resources::tests_resources::TEST_RESOURCES,
    shm::tests_shm::TEST_SHM,
    task::tests_task::TEST_TASK,
    time::tests_time::TEST_TIME,
    vfs::tests_vfs::TEST_VFS,
};

pub fn kcore_unit_test() {
    let mut runner = TestRunner::new();
    run_tests!(
        runner,
        [
            TEST_CONFIG,
            TEST_FUTEX,
            TEST_LRUCACHE,
            TEST_MM,
            TEST_RESOURCES,
            TEST_SHM,
            TEST_TASK,
            TEST_TIME,
            TEST_VFS,
        ]
    );

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }
}
