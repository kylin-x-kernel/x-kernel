// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::def_test;

use super::*;

#[def_test]
fn test_sock_extended_err_origin_matches_linux_abi() {
    assert_eq!(
        sock_extended_err_origin(SocketErrorOrigin::Local),
        SO_EE_ORIGIN_LOCAL
    );
    assert_eq!(
        sock_extended_err_origin(SocketErrorOrigin::Icmp),
        SO_EE_ORIGIN_ICMP
    );
    assert_eq!(
        sock_extended_err_origin(SocketErrorOrigin::Icmp6),
        SO_EE_ORIGIN_ICMP6
    );
    assert_eq!(
        sock_extended_err_origin(SocketErrorOrigin::TxStatus),
        SO_EE_ORIGIN_TXSTATUS
    );
}

#[def_test]
fn scm_rights_body_caps_at_scm_max_fd() {
    assert_eq!(
        CMsg::rights_fd_count(SCM_MAX_FD * size_of::<i32>()),
        Ok(SCM_MAX_FD)
    );
    assert_eq!(
        CMsg::rights_fd_count((SCM_MAX_FD + 1) * size_of::<i32>()),
        Err(KError::InvalidInput)
    );
    // A body that is not a whole number of descriptors is malformed.
    assert_eq!(
        CMsg::rights_fd_count(size_of::<i32>() - 1),
        Err(KError::InvalidInput)
    );
    assert_eq!(CMsg::rights_fd_count(0), Ok(0));
}

#[def_test]
fn sendmsg_descriptor_budget_caps_at_scm_max_fd() {
    let mut budget = ScmFdBudget::default();
    assert_eq!(budget.charge(SCM_MAX_FD - 1), Ok(()));
    assert_eq!(budget.charge(1), Ok(()));
    // The total across a `sendmsg` is capped, not only each message.
    assert_eq!(budget.charge(1), Err(KError::InvalidInput));
    assert_eq!(ScmFdBudget::default().charge(SCM_MAX_FD), Ok(()));
}
