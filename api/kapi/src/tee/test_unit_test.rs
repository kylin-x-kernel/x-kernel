// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

//! TEE unit test entry points.
use unittest::{
    run_tests,
    test_framework::{TestRunner, tests_failed},
};

use crate::tee::{
    tee_session::tests_tee_session::TEST_TEE_SESSION,
    user_access::tests_user_access::TEST_USER_ACCESS,
};

pub fn tee_unit_test() {
    warn!("********************************");
    warn!("Starting TEE unit tests...");

    let mut runner = TestRunner::new();
    run_tests!(runner, [TEST_TEE_SESSION, TEST_USER_ACCESS,]);

    if tests_failed() {
        error!("!!! SOME TESTS FAILED, NEED TO BE FIXED !!!");
    } else {
        warn!("!!! ALL TESTS PASSED !!!");
    }

    warn!("********************************\n");
}
