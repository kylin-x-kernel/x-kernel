// SPDX-License-Identifier: Apache-2.0
// Copyright 2025 KylinSoft Co., Ltd. <https://www.kylinos.cn/>
// See LICENSES for license details.

use unittest::{assert_eq, def_test};

use super::{sys_getpgid, sys_getsid, sys_setpgid};

#[def_test(user, serial)]
fn test_getsid_zero_targets_current_process() {
    let proc = kprocess::current_user_process();
    assert_eq!(
        sys_getsid(0).unwrap(),
        proc.group().session().sid() as isize
    );
}

#[def_test(user, serial)]
fn test_getpgid_zero_targets_current_process() {
    let proc = kprocess::current_user_process();
    assert_eq!(sys_getpgid(0).unwrap(), proc.group().pgid() as isize);
}

#[cfg(target_arch = "x86_64")]
#[def_test(user, serial)]
fn test_getpgrp_returns_current_process_group() {
    assert_eq!(super::sys_getpgrp().unwrap(), sys_getpgid(0).unwrap());
}

#[def_test(user, serial)]
fn test_setpgid_zero_zero_targets_current_process() {
    let proc = kprocess::current_user_process();
    let original_pgid = proc.group().pgid();

    assert_eq!(sys_setpgid(0, 0).unwrap(), 0);
    assert_eq!(proc.group().pgid(), original_pgid);
}
