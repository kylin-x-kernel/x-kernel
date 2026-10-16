// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use kcred::Cred;
use kerrno::KError;
use unittest::{assert, assert_eq, def_test};

use super::check_cross_process_read_real_creds_access;

#[def_test]
fn matching_real_credentials_allow_cross_process_read() {
    let caller = Cred::new(1000, 100);
    let target = Cred::new(1000, 100);

    assert!(check_cross_process_read_real_creds_access(&caller, &target).is_ok());
}

#[def_test]
fn mismatched_real_credentials_deny_cross_process_read() {
    let caller = Cred::new(1000, 100);
    let target = Cred::new(2000, 200);

    assert_eq!(
        check_cross_process_read_real_creds_access(&caller, &target),
        Err(KError::OperationNotPermitted)
    );
}

#[def_test]
fn privileged_credentials_allow_cross_process_read() {
    assert!(
        check_cross_process_read_real_creds_access(&Cred::root(), &Cred::new(1000, 100)).is_ok()
    );
}
